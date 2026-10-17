"""Repository harness unit tests. No native or official Bazel is launched."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import shutil
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import validate_repositories as repositories


def record(stdout="", stderr="", returncode=0, timed_out=False):
    return {"id": 0, "name": "mock", "stdout": stdout, "stderr": stderr,
            "returncode": returncode, "timed_out": timed_out}


class FixtureTests(unittest.TestCase):
    def test_rule_inventories_and_modules(self):
        for relative, names in (("p", repositories.ROOT_NAMES),
                                ("repos/alpha/p", repositories.ALPHA_NAMES),
                                ("repos/beta/p", ("copy",))):
            text = (repositories.FIXTURE / relative / "BUILD.bazel").read_text()
            actual = re.findall(r'^\w+\(name = "([^"]+)"', text, re.M)
            self.assertEqual(sorted(actual), sorted(names))
            self.assertEqual(len(actual), len(set(actual)))
        modules = sorted(path.relative_to(repositories.FIXTURE).as_posix()
                         for path in repositories.FIXTURE.rglob("MODULE.bazel"))
        self.assertEqual(modules, ["MODULE.bazel", "repos/alpha/MODULE.bazel", "repos/beta/MODULE.bazel"])

    def test_source_collision_inventory_has_different_bytes(self):
        manifests = repositories.artifact_manifest(
            [repositories.FIXTURE / root / "p/data.txt" for root in (".", "repos/alpha", "repos/beta")],
            repositories.SOURCES, repositories.FIXTURE)
        self.assertEqual(len(manifests), 3)
        self.assertEqual(len({item["sha256"] for item in manifests.values()}), 3)
        self.assertEqual({key.rsplit("/", 1)[1] for key in manifests}, {"data.txt"})

    def test_expected_metadata_preserves_repository_identity(self):
        for external in (False, True):
            expected = repositories.custom_expected(external)
            self.assertEqual(len(expected), 2)
            metadata = next(value for key, value in expected.items() if key.endswith("metadata.txt"))
            lines = metadata.decode().splitlines()
            self.assertEqual(len(lines), 4)
            self.assertEqual(lines[1], lines[2])
            self.assertEqual(lines[1].split("|"), [
                "external/fixture_alpha+/p/data.txt", "../fixture_alpha+/p/data.txt", "@@fixture_alpha+//p:data.txt"])
            self.assertEqual(lines[0], "@@fixture_alpha+//p:inspect" if external else "@@//p:inspect")
            self.assertNotIn("bazel-out", metadata.decode())

    def test_coverage_inventories(self):
        targets = [case[0] for case in repositories.OUTPUT_CASES]
        self.assertLess(targets.index("@first//p:hidden"), targets.index("//p:inspect"))
        self.assertLess(targets.index("@first//p:forwarded"), targets.index("//p:inspect"))
        self.assertEqual({target: count for target, _, count in repositories.OUTPUT_CASES if count},
                         {"//p:inspect": 2, "@first//p:inspect": 2})
        self.assertEqual({case[0] for case in repositories.ERROR_CASES}, {
            "root-dependency-alias", "external-root-alias", "external-empty-alias",
            "cross-repository-private", "external-pkg-not-main"})


class LabelTests(unittest.TestCase):
    def test_query_parser_preserves_exact_spelling(self):
        labels = ["//p:x", "@@//p:y", "@first//p:x", "@@fixture_alpha+//p:x"]
        self.assertEqual(repositories.parse_labels("\n".join(labels)), sorted(labels))
        for text in ("not a label", "@first/p:x", "@@repo//p:", "//p:x\n//p:x"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                repositories.parse_labels(text)

    def test_mapping_is_explicit_not_repository_name_stripping(self):
        self.assertEqual(repositories.canonical_label("@first//p:x"), "@@fixture_alpha+//p:x")
        self.assertEqual(repositories.canonical_label("@@//p:x"), "//p:x")
        self.assertNotEqual(repositories.canonical_label("@first//p:x"), repositories.canonical_label("@second//p:x"))
        self.assertNotEqual(repositories.canonical_label("@first//p:x"), "//p:x")
        with self.assertRaises(ValueError):
            repositories.canonical_label("@dep//p:x")

    def test_query_reports_spelling_difference_without_claiming_exact_parity(self):
        reference = mock.Mock(workspace=Path("/fixture"))
        reference.run.return_value = record("@first//p:copy\n")
        with mock.patch.object(repositories, "native_invoke", return_value=record("@@fixture_alpha+//p:copy\n")):
            details = repositories.query_parity(Path("/native"), reference, Path("/cache"), mock.Mock(),
                                                "@first//p:copy", ["@@fixture_alpha+//p:copy"])
        self.assertFalse(details["exact_output_equal"])
        self.assertEqual(details["exact_labels"]["reference"], ["@first//p:copy"])
        self.assertEqual(details["canonical_identities"]["native"], details["canonical_identities"]["reference"])
        for wrong in ("//p:copy\n", "@@fixture_beta+//p:copy\n", "@first//p:copy\n"):
            with mock.patch.object(repositories, "native_invoke", return_value=record(wrong)), self.assertRaises(ValueError):
                repositories.query_parity(Path("/native"), reference, Path("/cache"), mock.Mock(),
                                          "@first//p:copy", ["@@fixture_alpha+//p:copy"])

    def test_native_output_parser_checks_target_paths_and_duplicates(self):
        text = "@@fixture_alpha+//p:copy\t/cache/bazel-out/rust-fastbuild/bin/external/fixture_alpha+/p/copy.txt\n"
        self.assertEqual(len(repositories.native_paths(record(text), "@first//p:copy")), 1)
        for bad in (text + text, text.replace("fixture_alpha+//", "fixture_beta+//"),
                    "@@fixture_alpha+//p:copy\trelative.txt", "@@fixture_alpha+//p:copy\t/cache/../file", "bad"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                repositories.native_paths(record(bad), "@first//p:copy")


class PathEvidenceTests(unittest.TestCase):
    def test_only_explicit_configuration_prefixes_are_normalized(self):
        workspace = Path("/fixture")
        for configuration in ("rust-fastbuild", "k8-fastbuild"):
            path = Path("/cache/bazel-out") / configuration / "bin/external/fixture_alpha+/p/copy.txt"
            self.assertEqual(repositories.artifact_key(path, workspace), "generated:external/fixture_alpha+/p/copy.txt")
        for path in ("/cache/bazel-out/unknown/bin/p/copy.txt", "/cache/bazel-out/k8-fastbuild/lib/p/copy.txt",
                     "/cache/bazel-out/k8-fastbuild/bin/../copy.txt", "/escape/data.txt", "relative.txt"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                repositories.artifact_key(Path(path), workspace)

    def test_collision_safe_evidence_and_exact_bytes_modes(self):
        expected = repositories.copies_expected()
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            paths = []
            for key, data in expected.items():
                path = root / "bazel-out/rust-fastbuild/bin" / key.split(":", 1)[1]
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
                path.chmod(0o640)
                paths.append(path)
            manifest = repositories.artifact_manifest(paths, expected, root / "fixture")
            self.assertEqual(set(manifest), set(expected))
            for key, value in manifest.items():
                self.assertEqual(base64.b64decode(value["bytes_base64"]), expected[key])
                self.assertEqual(value["sha256"], hashlib.sha256(expected[key]).hexdigest())
                self.assertEqual(value["size_bytes"], len(expected[key]))
                self.assertEqual(value["mode_octal"], "0640")
            for bad in (paths[:1], paths + paths[:1]):
                with self.assertRaises(ValueError):
                    repositories.artifact_manifest(bad, expected, root)
            paths[1].write_bytes(b"wrong repository\n")
            with self.assertRaisesRegex(ValueError, "unexpected output bytes"):
                repositories.artifact_manifest(paths, expected, root)

    def test_external_source_symlink_resolves_to_authorized_override_only(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            workspace = root / "fixture"
            source = workspace / "repos/alpha/p/data.txt"
            source.parent.mkdir(parents=True)
            source.write_bytes(b"alpha repository\n")
            external = root / "execroot/external/fixture_alpha+"
            external.parent.mkdir(parents=True)
            external.symlink_to(source.parent.parent, target_is_directory=True)
            key = repositories.artifact_key(external / "p/data.txt", workspace)
            self.assertEqual(key, "source:external/fixture_alpha+/p/data.txt")
            outside = root / "outside"
            outside.write_text("outside")
            link = workspace / "bad.txt"
            link.symlink_to(outside)
            with self.assertRaisesRegex(ValueError, "escaped"):
                repositories.artifact_key(link, workspace)


class InvocationAndErrorTests(unittest.TestCase):
    def test_defaults_timeout_and_direct_invocation(self):
        args = repositories.argument_parser().parse_args([])
        self.assertEqual(args.timeout, 90)
        self.assertEqual(args.output.name, "repository-validation.json")
        for text in ("0", "-1", "nan", "inf"):
            with self.assertRaises(argparse.ArgumentTypeError):
                repositories.positive_seconds(text)
        runner = mock.Mock()
        runner.run.return_value = record()
        repositories.native_invoke(Path("/native"), Path("/cache"), runner, Path("/fixture"), "build", "build", "@first//p:copy", 4)
        self.assertEqual(runner.run.call_args.args[1], [
            "/native", "--ignore_all_rc_files", "--cache-dir=/cache", "build", "--lockfile_mode=off", "--jobs=4",
            "@first//p:copy"])
        runner.run.return_value = record(stderr="falling back to official")
        with self.assertRaisesRegex(ValueError, "fallback"):
            repositories.native_invoke(Path("/native"), Path("/cache"), runner, Path("/fixture"), "q", "query", "//p:all")

    def test_official_wrapper_rejected(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            executable = Path(directory) / "wrapper"
            executable.write_text("#!/bin/sh\nexit 0\n")
            executable.chmod(0o755)
            with self.assertRaisesRegex(ValueError, "official Linux ELF"):
                repositories.validate_executable(executable, official=True)
            executable.chmod(0o644)
            with self.assertRaisesRegex(ValueError, "not executable"):
                repositories.validate_executable(executable)

    def test_reference_has_private_output_and_download_caches(self):
        runner = mock.Mock()
        reference = repositories.RepositoryReference(Path("/official"), Path("/fixture"), Path("/scratch"), runner, "r")
        reference.run("q", "query", "@first//p:all")
        argv = runner.run.call_args.args[1]
        for flag in ("--ignore_all_rc_files", "--output_base=/scratch/r-output-base",
                     "--repository_cache=/scratch/repository-cache", "--repo_contents_cache=/scratch/repository-contents"):
            self.assertIn(flag, argv)
        self.assertFalse(any("override_repository" in arg or "override_module" in arg for arg in argv))

    def test_semantic_errors_never_accept_crashes_timeouts_or_path_names(self):
        pattern = repositories.MAPPING_ERROR
        good = record(stderr="No repository visible as '@dep' from repository '@@'", returncode=1)
        self.assertEqual(repositories.semantic_error(good, pattern)["status"], "EXPECTED_FAILURE")
        native = record(stderr='unknown apparent repository @dep from canonical repository "fixture_alpha+"', returncode=1)
        self.assertEqual(repositories.semantic_error(native, pattern)["status"], "EXPECTED_FAILURE")
        for change in ({"returncode": 0}, {"returncode": None}, {"returncode": -9}, {"timed_out": True},
                       {"stderr": "unknown unsupported API"}, {"stdout": "//p:x"},
                       {"stderr": "FATAL: unknown repository"}, {"stderr": "panicked at unknown repo"}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                repositories.semantic_error(dict(good, **change), pattern)
        for category, _, _, pattern in repositories.ERROR_CASES:
            with self.assertRaises(ValueError):
                repositories.semantic_error(record(stderr="unsupported API /errors/" + category + "/BUILD.bazel", returncode=1), pattern)

    def test_one_sided_rejections_fail(self):
        reference = mock.Mock(workspace=Path("/fixture"))
        failure = record(stderr="unknown repository dep", returncode=1)
        for native, official in ((record(), failure), (failure, record())):
            reference.run.return_value = official
            with mock.patch.object(repositories, "native_invoke", return_value=native), self.assertRaises(ValueError):
                repositories.error_parity(Path("/native"), reference, Path("/cache"), mock.Mock(), repositories.ERROR_CASES[0])


class InvalidationTests(unittest.TestCase):
    def test_three_cold_three_warm_one_invalidated_and_source_restored(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            scratch = Path(directory)
            workspace = scratch / "fixture"
            shutil.copytree(repositories.FIXTURE, workspace)
            reference = mock.Mock(workspace=workspace)
            seen = []

            def pair(native, ref, cache, runner, target, expected, counts, name):
                seen.append((counts, cache, (workspace / "repos/alpha/p/data.txt").read_bytes()))
                self.assertEqual(expected, repositories.copies_expected(seen[-1][2]))
                return {}

            with mock.patch.object(repositories, "build_pair", side_effect=pair):
                repositories.collision_builds(Path("/native"), reference, scratch, mock.Mock())
            self.assertEqual([item[0] for item in seen], [(3, 0), (0, 3), (1, 2)])
            self.assertEqual(len({item[1] for item in seen}), 1)
            self.assertEqual(seen[-1][2], b"changed alpha repository\n")
            self.assertEqual((workspace / "repos/alpha/p/data.txt").read_bytes(), b"alpha repository\n")

    def test_interruption_restores_private_source(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            scratch = Path(directory)
            workspace = scratch / "fixture"
            shutil.copytree(repositories.FIXTURE, workspace)
            reference = mock.Mock(workspace=workspace)
            with mock.patch.object(repositories, "build_pair", side_effect=[{}, {}, KeyboardInterrupt]), self.assertRaises(KeyboardInterrupt):
                repositories.collision_builds(Path("/native"), reference, scratch, mock.Mock())
            self.assertEqual((workspace / "repos/alpha/p/data.txt").read_bytes(), b"alpha repository\n")

    def test_tracked_source_rejected(self):
        with self.assertRaisesRegex(ValueError, "private fixture copy"):
            repositories.private_source(mock.Mock(workspace=repositories.FIXTURE), Path("/tmp/opencode"))

    def test_action_counts_are_measured(self):
        self.assertEqual(repositories.action_counts(record(stderr="3 actions executed, 0 cache hits"), (3, 0)),
                         {"actions_executed": 3, "cache_hits": 0})
        for result in (record(), record(stderr="2 actions executed, 1 cache hits")):
            with self.assertRaises(ValueError):
                repositories.action_counts(result, (3, 0))


class LifecycleTests(unittest.TestCase):
    def run_main(self, output, release="release 9.3.0\n", shutdown_error=None, interrupt=False, failed_check=False):
        reference = mock.Mock()
        reference.run.return_value = record(release)
        reference.shutdown.side_effect = shutdown_error
        query_effect = KeyboardInterrupt if interrupt else (ValueError("query mismatch") if failed_check else None)
        with mock.patch.object(repositories, "validate_executable"), \
                mock.patch.object(repositories, "file_evidence", return_value={}), \
                mock.patch.object(repositories, "RepositoryReference", return_value=reference), \
                mock.patch.object(repositories, "query_parity", side_effect=query_effect, return_value={}), \
                mock.patch.object(repositories, "collision_builds", return_value={}), \
                mock.patch.object(repositories, "output_case", return_value={}), \
                mock.patch.object(repositories, "error_parity", return_value={}), \
                mock.patch.object(repositories, "registry_case", return_value={}), \
                mock.patch.object(repositories.Runner, "run", side_effect=AssertionError("no subprocess allowed")), \
                mock.patch("builtins.print"):
            code = repositories.main(["--output", str(output)])
        reference.shutdown.assert_called_once()
        report = json.loads(output.read_text())
        self.assertTrue(report["scratch_cleaned"])
        self.assertFalse(Path(report["scratch_directory"]).exists())
        self.assertIn("cache_sizes_bytes_before_cleanup", report)
        return code, report

    def test_success_requires_every_check(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            code, report = self.run_main(Path(directory) / "report.json")
        self.assertEqual(code, 0)
        self.assertEqual(report["status"], "PASS")
        self.assertEqual(len(report["checks"]), repositories.expected_check_count())
        self.assertEqual(len({check["name"] for check in report["checks"]}), len(report["checks"]))

    def test_rejected_release_failed_check_shutdown_and_interrupt_all_cleanup(self):
        for options, expected_code, error in (({"release": "release 9.1.1\n"}, 1, "reference binary"),
                                               ({"shutdown_error": ValueError("shutdown failed")}, 1, "shutdown:"),
                                               ({"interrupt": True}, 130, "interrupted"),
                                               ({"failed_check": True}, 1, None)):
            with self.subTest(options=options), tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
                code, report = self.run_main(Path(directory) / "report.json", **options)
                self.assertEqual(code, expected_code)
                self.assertEqual(report["status"], "FAIL")
                if error:
                    self.assertTrue(any(error in item for item in report["errors"]))
                else:
                    self.assertTrue(any(item["status"] == "FAIL" for item in report["checks"]))

    def test_output_guard_runs_before_any_process(self):
        with mock.patch.object(repositories, "validate_executable") as guard, self.assertRaisesRegex(ValueError, "outside tracked fixtures"):
            repositories.main(["--output", str(repositories.FIXTURE / "report.json")])
        guard.assert_not_called()


class RegistryTests(unittest.TestCase):
    def test_registry_metadata_selects_source_and_is_authoritative(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            workspace, registry, metadata = repositories.registry_fixture(root)
            source = json.loads((metadata / "source.json").read_text())
            self.assertEqual(source, {"type": "local_path", "path": str(workspace / "repos/alpha")})
            self.assertNotEqual(metadata, Path(source["path"]))
            self.assertNotIn("bazel_dep", (metadata / "MODULE.bazel").read_text())
            self.assertIn("bazel_dep", (workspace / "repos/alpha/MODULE.bazel").read_text())
            self.assertNotIn("local_path_override", (workspace / "MODULE.bazel").read_text())

    def test_registry_flags_are_local_for_native_bcr_fallback_only_for_official(self):
        runner = mock.Mock()
        runner.run.return_value = record()
        repositories.native_invoke(Path("/native"), Path("/cache"), runner, Path("/fixture"),
                                   "registry", "build", "@first//p:copy", registry="file:///registry")
        native_argv = runner.run.call_args.args[1]
        self.assertIn("--registry=file:///registry", native_argv)
        self.assertIn("--jobs=4", native_argv)
        self.assertFalse(any("https:" in arg for arg in native_argv))
        reference = repositories.RepositoryReference(Path("/official"), Path("/fixture"), Path("/scratch"), runner, "registry")
        reference.registry_url = "file:///registry"
        reference.run("q", "query", "@first//p:copy")
        argv = runner.run.call_args.args[1]
        self.assertEqual([arg for arg in argv if arg.startswith("--registry=")],
                         ["--registry=file:///registry", "--registry=https://bcr.bazel.build"])

    def test_registry_cold_warm_change_counts_and_shutdown(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            reference = mock.Mock(workspace=root / "fixture")
            seen = []

            def pair(native, ref, cache, runner, target, expected, counts, name, registry):
                source = (ref.workspace / "repos/alpha/p/data.txt").read_bytes()
                self.assertEqual(expected["generated:external/fixture_alpha+/p/copy.txt"], source)
                seen.append(counts)
                return {}

            with mock.patch.object(repositories, "RepositoryReference", return_value=reference), \
                    mock.patch.object(repositories, "query_parity", return_value={}), \
                    mock.patch.object(repositories, "build_pair", side_effect=pair), \
                    mock.patch.object(repositories, "error_parity", return_value={}):
                repositories.registry_case(Path("/native"), Path("/official"), root, mock.Mock())
            reference.shutdown.assert_called_once()
            self.assertEqual(seen, [(1, 0), (0, 1), (1, 0)])
            self.assertEqual((reference.workspace / "repos/alpha/p/data.txt").read_bytes(), b"alpha repository\n")

    def test_registry_failure_still_shuts_down(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            reference = mock.Mock(workspace=root / "fixture")
            with mock.patch.object(repositories, "RepositoryReference", return_value=reference), \
                    mock.patch.object(repositories, "query_parity", side_effect=KeyboardInterrupt), \
                    self.assertRaises(KeyboardInterrupt):
                repositories.registry_case(Path("/native"), Path("/official"), root, mock.Mock())
            reference.shutdown.assert_called_once()


if __name__ == "__main__":
    unittest.main()
