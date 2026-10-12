"""Stdlib helper/mocked tests: never launch either Bazel implementation."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import validate_loading as loading


def record(stdout="", stderr="", returncode=0, timed_out=False):
    return {"id": 0, "name": "mock", "stdout": stdout, "stderr": stderr,
            "returncode": returncode, "timed_out": timed_out}


class ParserTests(unittest.TestCase):
    def test_defaults(self):
        args = loading.argument_parser().parse_args([])
        self.assertEqual(args.timeout, 90)
        self.assertEqual(args.output.name, "loading-validation.json")
        self.assertFalse(args.skip_repo)

    def test_invalid_timeout(self):
        for text in ("0", "-1", "nan", "inf"):
            with self.subTest(text=text), self.assertRaises(argparse.ArgumentTypeError):
                loading.positive_seconds(text)

    def test_jobs_command_is_explicit_and_does_not_use_wrapper(self):
        runner = mock.Mock()
        loading.native_invoke(Path("/native"), Path("/cache"), runner, Path("/repo"),
                              "build", "build", "//:flattened", 4)
        self.assertEqual(runner.run.call_args.args[1], [
            "/native", "--ignore_all_rc_files", "--cache-dir=/cache", "build", "--jobs=4", "//:flattened"])

    def test_jobs_not_passed_to_query(self):
        runner = mock.Mock()
        loading.native_invoke(Path("/native"), Path("/cache"), runner, Path("/repo"), "q", "query", "//:all", 4)
        self.assertFalse(any(arg.startswith("--jobs") for arg in runner.run.call_args.args[1]))


class EvidenceTests(unittest.TestCase):
    def test_counts_must_be_measured(self):
        self.assertEqual(loading.action_counts(record(stderr="6 actions executed, 0 cache hits"), (6, 0)),
                         {"actions_executed": 6, "cache_hits": 0})
        for result in (record(), record(stderr="0 actions executed, 6 cache hits"),
                       record(stderr="6 actions executed, 0 cache hits", timed_out=True)):
            with self.assertRaises(ValueError):
                loading.action_counts(result, (6, 0))

    def test_expected_errors_reject_timeouts_and_unrelated_failures(self):
        self.assertEqual(loading.expected_error(record(stderr="load cycle", returncode=1), "cycle")["status"],
                         "EXPECTED_FAILURE")
        for result in (record(stderr="cycle"), record(stderr="cycle", returncode=None),
                       record(stderr="cycle", returncode=-9), record(stderr="network", returncode=1),
                       record(stderr="cycle", returncode=1, timed_out=True),
                       record(stdout="//:unexpected\n", stderr="cycle", returncode=1)):
            with self.assertRaises(ValueError):
                loading.expected_error(result, "cycle")

    def test_manifest_only_records_paths_sizes_hashes(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            path = root / "secret.yaml"
            path.write_bytes(b"not for the report")
            result = loading.source_manifest([path], root, 1, ".yaml")
            self.assertEqual(result["secret.yaml"], {
                "path": "secret.yaml", "size_bytes": 18,
                "sha256": hashlib.sha256(b"not for the report").hexdigest()})
            self.assertNotIn("not for the report", json.dumps(result))
            for paths, count, suffix in (([], None, None), ([path], 7, None),
                                         ([path], 1, ".txt"), ([path, path], None, None)):
                with self.assertRaises(ValueError):
                    loading.source_manifest(paths, root, count, suffix)

    def test_manifest_rejects_generated_external_and_symlink_escape(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory) / "repo"
            root.mkdir()
            outside = Path(directory) / "outside"
            outside.write_text("outside")
            escape = root / "escape"
            escape.symlink_to(outside)
            for name in ("bazel-out", "external", "bazel-bin"):
                generated = root / name / "file"
                generated.parent.mkdir()
                generated.write_text("generated")
                with self.assertRaises(ValueError):
                    loading.source_manifest([generated], root)
            for path in (outside, escape, root):
                with self.assertRaises(ValueError):
                    loading.source_manifest([path], root)

    def test_fixture_bytes_and_duplicate_basenames(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            path = Path(directory) / "a.txt"
            path.write_bytes(b"alpha\n")
            result = loading.fixture_outputs([path], {"a.txt": b"alpha\n"})
            self.assertIn("bytes_base64", result[0])
            for paths, expected in (([path, path], {"a.txt": b"alpha\n"}),
                                    ([path], {"a.txt": b"wrong"}), ([path], {"b.txt": b"alpha\n"})):
                with self.assertRaises(ValueError):
                    loading.fixture_outputs(paths, expected)

    def test_fixture_builds_execute_both_cold_job_counts_and_rebuilds(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            a, diamond = root / "a.txt", root / "diamond.txt"
            a.write_bytes(b"alpha\n")
            diamond.write_bytes(b"alpha\nalpha\nbeta\n")
            stdout = "//:flattened\t{}\n//:flattened\t{}\n".format(a, diamond)
            runner = mock.Mock()
            runner.run.side_effect = [record(stdout, counts) for counts in (
                "6 actions executed, 0 cache hits", "0 actions executed, 6 cache hits",
                "6 actions executed, 0 cache hits", "0 actions executed, 6 cache hits")]
            reference = mock.Mock(workspace=root)
            reference.run.return_value = record()
            with mock.patch.object(loading, "output_listing", return_value=[diamond, a]):
                details = loading.fixture_builds(Path("/native"), reference, root, runner)
            self.assertEqual([item["jobs"] for item in details["native"]], [1, 1, 4, 4])
            self.assertEqual(len(details["reference"]), 2)
            caches = [item["cache_directory"] for item in details["native"]]
            self.assertEqual(caches[0], caches[1])
            self.assertEqual(caches[2], caches[3])
            self.assertNotEqual(caches[0], caches[2])

    def test_source_preflight_rejects_generated_before_build(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            reference = mock.Mock(workspace=root)
            runner = mock.Mock()
            with mock.patch.object(loading, "query_parity", return_value={}), \
                    mock.patch.object(loading, "output_listing", return_value=[root / "bazel-out/generated"]):
                with self.assertRaises(ValueError):
                    loading.source_parity(Path("/native"), reference, root, runner, "//:unsafe")
            runner.run.assert_not_called()
            reference.run.assert_not_called()

    def test_source_parity_zero_actions_and_hash_equality(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            path = root / "data.yaml"
            path.write_text("sensitive yaml")
            reference = mock.Mock(workspace=root)
            reference.run.return_value = record()
            runner = mock.Mock()
            runner.run.return_value = record("//:files\t" + str(path), "0 actions executed, 0 cache hits")
            with mock.patch.object(loading, "query_parity", return_value={}), \
                    mock.patch.object(loading, "output_listing", return_value=[path]):
                details = loading.source_parity(Path("/native"), reference, root, runner, "//:files", 1, ".yaml")
            self.assertEqual(details["native"], details["reference"])
            self.assertTrue(details["source_only"])
            self.assertNotIn("sensitive yaml", json.dumps(details))


class RepositoryTests(unittest.TestCase):
    def test_git_snapshot_redacts_diffs_and_hashes_exact_bytes(self):
        records = [record(" M file\0"), record("secret diff"), record("replacement")]
        records[-1]["stdout_base64"] = base64.b64encode(b"\xffsecret").decode()
        runner = mock.Mock()
        runner.run.side_effect = records
        result = loading.git_snapshot(runner, Path("/repo"), "before")
        self.assertEqual(result["status_porcelain"], " M file\0")
        self.assertEqual(result["staged_diff_sha256"], hashlib.sha256(b"\xffsecret").hexdigest())
        self.assertNotIn("secret", json.dumps(records))
        self.assertTrue(all("--no-optional-locks" in call.args[1] for call in runner.run.call_args_list))

    def test_git_failure_still_redacts_stdout(self):
        failed = record("secret diff", returncode=1)
        runner = mock.Mock()
        runner.run.return_value = failed
        with self.assertRaises(ValueError):
            loading.git_snapshot(runner, Path("/repo"), "before")
        self.assertNotIn("secret", failed["stdout"])

    def test_state_comparison_runs_even_on_interrupt_never_reverts(self):
        report = {"checks": [], "errors": []}
        reference = mock.Mock(workspace=Path("/repo"))
        with mock.patch.object(loading, "git_snapshot", side_effect=[{"hash": "a"}, {"hash": "b"}]) as snapshot, \
                mock.patch.object(loading, "source_parity", side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                loading.repository_checks(report, Path("/native"), reference, Path("/cache"), mock.Mock())
        self.assertEqual(snapshot.call_count, 2)
        self.assertFalse(report["repository_state"]["unchanged"])
        self.assertIn("no edits reverted", report["errors"][0])

    def test_repository_allowlist_is_source_only(self):
        targets = [target for target, _, _ in loading.REPO_TARGETS]
        self.assertEqual(len(targets), 6)
        self.assertIn("//bin:bw-devenv", targets)
        self.assertFalse(any(target.endswith(":all") or "..." in target for target in targets))


class LifecycleTests(unittest.TestCase):
    def test_wrong_official_release_fails_and_shuts_down_private_server(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            output = Path(directory) / "report.json"
            reference = mock.Mock()
            reference.run.return_value = record("release 9.1.1\n")
            with mock.patch.object(loading, "validate_executable"), \
                    mock.patch.object(loading, "file_evidence", return_value={}), \
                    mock.patch.object(loading, "Reference", return_value=reference), \
                    mock.patch("builtins.print"), \
                    mock.patch.object(loading.Runner, "run") as run:
                self.assertEqual(loading.main(["--skip-repo", "--output", str(output)]), 1)
            report = json.loads(output.read_text())
            self.assertIn("reference binary is not official release 9.3.0", report["errors"])
            self.assertTrue(report["scratch_cleaned"])
            self.assertFalse(Path(report["scratch_directory"]).exists())
            reference.shutdown.assert_called_once()
            run.assert_not_called()

    def test_report_must_not_modify_probed_repository(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            output = Path(directory) / "new" / "report.json"
            with self.assertRaisesRegex(ValueError, "outside"):
                loading.main(["--repo", directory, "--output", str(output)])
            self.assertFalse(output.parent.exists())


if __name__ == "__main__":
    unittest.main()
