"""Stdlib-only tests; do not require or launch either Bazel executable."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    "validate", Path(__file__).resolve().parents[1] / "scripts" / "validate.py")
validate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validate)


class StatisticsTests(unittest.TestCase):
    def test_percentile_interpolates_and_handles_singleton(self):
        self.assertAlmostEqual(validate.percentile([4, 1, 3, 2], 95), 3.85)
        self.assertEqual(validate.percentile([7], 95), 7)
        self.assertEqual(validate.percentile([1, 2], 0), 1)
        self.assertEqual(validate.percentile([1, 2], 100), 2)

    def test_invalid_samples(self):
        for samples, percent in (([], 95), ([1], -1), ([1], 101),
                                 ([float("nan")], 95), ([float("inf")], 95), ([-1], 95)):
            with self.subTest(samples=samples, percent=percent), self.assertRaises(ValueError):
                validate.percentile(samples, percent)

    def test_summary_retains_raw_order(self):
        result = validate.summarize([3, 1, 2])
        self.assertEqual(result["count"], 3)
        self.assertEqual(result["median_seconds"], 2)
        self.assertAlmostEqual(result["p95_seconds"], 2.9)
        self.assertEqual(result["min_seconds"], 1)
        self.assertEqual(result["max_seconds"], 3)
        self.assertEqual(result["samples_seconds"], [3, 1, 2])


class ParserTests(unittest.TestCase):
    def test_positive_arguments(self):
        self.assertEqual(validate.positive_int("10"), 10)
        self.assertEqual(validate.positive_seconds("0.25"), 0.25)
        for text in ("0", "-1", "1.5", "nan", "no"):
            with self.subTest(text=text), self.assertRaises(argparse.ArgumentTypeError):
                validate.positive_int(text)
        for text in ("0", "-1", "nan", "inf", "-inf", "no"):
            with self.subTest(text=text), self.assertRaises(argparse.ArgumentTypeError):
                validate.positive_seconds(text)

    def test_defaults(self):
        args = validate.argument_parser().parse_args([])
        self.assertEqual(args.iterations, 10)
        self.assertEqual(args.timeout, 90)
        self.assertEqual(args.repo, Path.home() / "code")
        self.assertFalse(args.skip_repo)

    def test_sorted_labels(self):
        self.assertEqual(validate.parse_labels("//:upper\n//:copy\n\n"), ["//:copy", "//:upper"])
        for text in ("warning: nope\n", "//:copy\n//:copy\n", "//missing-colon\n"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                validate.parse_labels(text)

    def test_native_multiple_outputs_and_spaces(self):
        outputs = validate.parse_native_outputs("//:a\t/tmp/a\n//:a\t/tmp/b c\n")
        self.assertEqual(outputs, [("//:a", Path("/tmp/a")), ("//:a", Path("/tmp/b c"))])

    def test_native_malformed_outputs(self):
        for text in ("//:a /tmp/a", "//:a\trelative", "bad\t/tmp/a",
                     "//:a\t/tmp/a\tmore", "//:a\t/tmp/a\n//:a\t/tmp/a\n"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                validate.parse_native_outputs(text)

    def test_reference_source_and_generated_paths(self):
        self.assertEqual(validate.reference_output_paths(
            "src/docker/config.json\nbazel-out/k8-fastbuild/bin/upper.txt\n",
            Path("/repo"), Path("/scratch/base")), [
                Path("/repo/src/docker/config.json"),
                Path("/scratch/base/execroot/_main/bazel-out/k8-fastbuild/bin/upper.txt")])
        for text in ("/absolute", "../escape", "bazel-out/../../escape", "a\na\n", "."):
            with self.subTest(text=text), self.assertRaises(ValueError):
                validate.reference_output_paths(text, Path("/repo"), Path("/base"))


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.report = {"runs": [], "cleanup": [], "errors": [], "unsupported": []}
        self.runner = validate.Runner(self.report, 0.5)

    def test_environment_drops_wrapper_settings(self):
        with mock.patch.dict(os.environ, {"BAZELRC": "bad", "USE_BAZEL_VERSION": "wrong"}):
            env = validate.clean_environment()
        self.assertNotIn("BAZELRC", env)
        self.assertNotIn("USE_BAZEL_VERSION", env)
        self.assertEqual(env["DO_NOT_TRACK"], "1")

    @mock.patch.object(validate.subprocess, "Popen")
    def test_run_records_exact_command_and_output(self, popen):
        process = popen.return_value
        process.communicate.return_value = (b"//:upper\n", b"message\n")
        process.returncode = 0
        result = self.runner.run("sample", ["/bin/native", "query", "//:all"], Path("/repo"))
        self.assertEqual(result["argv"], ["/bin/native", "query", "//:all"])
        self.assertEqual(result["cwd"], "/repo")
        self.assertEqual(result["stdout"], "//:upper\n")
        self.assertEqual(result["stderr"], "message\n")
        self.assertGreaterEqual(result["elapsed_seconds"], 0)
        self.assertEqual(result["returncode"], 0)
        self.assertTrue(popen.call_args.kwargs["start_new_session"])
        self.assertEqual(popen.call_args.kwargs["stdin"], subprocess.DEVNULL)

    @mock.patch.object(validate.os, "killpg")
    @mock.patch.object(validate.subprocess, "Popen")
    def test_timeout_kills_whole_group_even_when_leader_exited(self, popen, killpg):
        process = popen.return_value
        process.pid = 2345
        process.returncode = 0
        process.communicate.side_effect = [subprocess.TimeoutExpired(["x"], 0.5),
                                           (b"partial\n", b"error\n")]
        result = self.runner.run("timeout", ["/bin/x"], Path("/repo"))
        killpg.assert_called_once_with(2345, signal.SIGKILL)
        self.assertTrue(result["timed_out"])
        self.assertEqual(result["stdout"], "partial\n")
        with self.assertRaises(ValueError):
            validate.require_success(result)

    @mock.patch.object(validate.os, "killpg")
    @mock.patch.object(validate.subprocess, "Popen")
    def test_interrupt_is_logged_and_kills_group(self, popen, killpg):
        process = popen.return_value
        process.pid = 1234
        process.returncode = -9
        process.communicate.side_effect = [KeyboardInterrupt, (b"partial", b"")]
        with self.assertRaises(KeyboardInterrupt):
            self.runner.run("interrupted", ["x"], Path("/repo"))
        killpg.assert_called_once_with(1234, signal.SIGKILL)
        self.assertTrue(self.report["runs"][0]["interrupted"])
        self.assertEqual(self.report["runs"][0]["stdout"], "partial")

    @mock.patch.object(validate.subprocess, "Popen", side_effect=FileNotFoundError("missing"))
    def test_spawn_failure_retained(self, popen):
        result = self.runner.run("missing", ["missing"], Path("/repo"))
        self.assertIsNone(result["returncode"])
        self.assertEqual(result["spawn_error"], "missing")

    @mock.patch.object(validate.subprocess, "Popen")
    def test_invalid_utf8_is_lossless_in_base64(self, popen):
        popen.return_value.communicate.return_value = (b"\xff", b"")
        popen.return_value.returncode = 0
        result = self.runner.run("bytes", ["x"], Path("/repo"))
        self.assertEqual(result["stdout_base64"], "/w==")

    def test_reference_flags_match_bazel_93(self):
        reference = validate.Reference(Path("/bazel"), Path("/repo"), Path("/scratch"), self.runner, "fixture")
        passed = {"id": 0, "returncode": 0, "timed_out": False, "stderr": ""}
        with mock.patch.object(self.runner, "run", return_value=passed) as run:
            self.assertIs(reference.run("q", "query", "//:all"), passed)
        run.assert_called_once()
        first = run.call_args_list[0].args[1]
        self.assertIn("--ignore_all_rc_files", first)
        self.assertIn("--output_base=/scratch/fixture-output-base", first)
        self.assertIn("--output_user_root=/scratch/fixture-output-user-root", first)
        self.assertIn("--lockfile_mode=off", first)
        self.assertIn("--remote_cache=", first)
        self.assertIn("--remote_executor=", first)
        self.assertIn("--bes_backend=", first)
        self.assertIn("--remote_upload_local_results=false", first)
        self.assertIn("--repo_env=DO_NOT_TRACK=1", first)
        self.assertNotIn("--autoload_scl_symbols=", first)

    def test_reference_does_not_retry_other_errors(self):
        reference = validate.Reference(Path("/bazel"), Path("/repo"), Path("/scratch"), self.runner, "fixture")
        with mock.patch.object(self.runner, "run", return_value={
                "returncode": 1, "timed_out": False, "stderr": "network failed"}) as run:
            reference.run("b", "build", "//:upper")
        run.assert_called_once()
        self.assertIn("--experimental_convenience_symlinks=ignore", run.call_args.args[1])

    def test_shutdown_only_uses_private_output_base(self):
        reference = validate.Reference(Path("/bazel"), Path("/repo"), Path("/scratch"), self.runner, "fixture")
        reference.started = True
        with mock.patch.object(self.runner, "run", return_value={"returncode": 0, "timed_out": False}) as run:
            reference.shutdown()
        self.assertIn("shutdown", run.call_args.args[1])
        self.assertIn("--output_base=/scratch/fixture-output-base", run.call_args.args[1])

    def test_shutdown_refuses_unrelated_pid(self):
        reference = validate.Reference(Path("/bazel"), Path("/repo"), Path("/scratch"), self.runner, "fixture")
        reference.started = True
        with mock.patch.object(self.runner, "run", return_value={"returncode": 1, "timed_out": True}), \
                mock.patch.object(Path, "read_text", return_value="2345"), \
                mock.patch.object(Path, "read_bytes", return_value=b"java\0--output_base=/other\0"), \
                mock.patch.object(validate.os, "kill") as kill, \
                mock.patch.object(validate.os, "killpg") as killpg:
            reference.shutdown()
        kill.assert_not_called()
        killpg.assert_not_called()
        self.assertIn("unverified", self.report["errors"][0])

    def test_unsupported_requires_diagnostic_not_timeout(self):
        for timed_out, stderr, expected in (
                (False, "load statements are not supported", "EXPECTED_FAILURE"),
                (False, "load() is unsupported; no .bzl loader is installed", "EXPECTED_FAILURE"),
                (True, "load statements are not supported", "UNEXPECTED_RESULT"),
                (False, "missing package", "UNEXPECTED_RESULT")):
            with self.subTest(timed_out=timed_out, stderr=stderr):
                result = {"id": 0, "returncode": 1, "timed_out": timed_out, "stdout": "", "stderr": stderr}
                with mock.patch.object(self.runner, "run", return_value=result):
                    validate.unsupported_probe(Path("/native"), Path("/repo"), Path("/cache"), self.runner)
                self.assertEqual(self.report["unsupported"][-1]["status"], expected)


class EvidenceTests(unittest.TestCase):
    def test_native_cache_reuse_must_be_observed_not_assumed(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            path = Path(directory) / "upper.txt"
            path.write_bytes(validate.EXPECTED_UPPER)
            runner = mock.Mock()
            runner.run.return_value = {
                "id": 0, "returncode": 0, "timed_out": False,
                "stdout": "//:upper\t" + str(path) + "\n",
                "stderr": "2 actions executed, 0 cache hits",
            }
            reference = mock.Mock(workspace=Path(directory))
            with self.assertRaisesRegex(ValueError, "execute/cache"):
                validate.fixture_builds(Path("/native"), reference, Path(directory), runner)
            self.assertEqual(runner.run.call_count, 2)
            reference.run.assert_not_called()

    def test_actual_bytes_verified(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            path = Path(directory) / "upper.txt"
            path.write_bytes(validate.EXPECTED_UPPER)
            evidence = validate.file_evidence(path, validate.EXPECTED_UPPER, True)
            self.assertEqual(evidence["size_bytes"], len(validate.EXPECTED_UPPER))
            self.assertIn("bytes_base64", evidence)
            path.write_bytes(b"wrong\n")
            with self.assertRaises(ValueError):
                validate.file_evidence(path, validate.EXPECTED_UPPER)

    def test_report_is_readable_and_atomic(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            path = Path(directory) / "validation.json"
            validate.write_report(path, {"status": "FAIL", "runs": []})
            self.assertEqual(json.loads(path.read_text())["status"], "FAIL")
            self.assertIn("\n  ", path.read_text())
            self.assertEqual(list(path.parent.iterdir()), [path])

    def test_query_benchmark_excludes_first_sample(self):
        report = {"runs": [], "benchmarks": {}}

        def invoke(name):
            record = {"id": len(report["runs"]), "returncode": 0, "timed_out": False,
                      "stdout": "//:a\n", "elapsed_seconds": 100 if not report["runs"] else 2}
            report["runs"].append(record)
            return record

        validate.query_benchmark(report, "native", invoke, ["//:a"], 3)
        benchmark = report["benchmarks"]["native"]
        self.assertEqual(benchmark["first_invocation"]["seconds"], 100)
        self.assertEqual(benchmark["warm"]["samples_seconds"], [2, 2, 2])
        self.assertEqual(benchmark["run_ids"], [1, 2, 3])

    def test_failure_cannot_be_a_parity_pass(self):
        report = {"checks": []}

        def fail():
            raise ValueError("not parity")

        validate.check(report, "fixture", fail)
        self.assertEqual(report["checks"][0]["status"], "FAIL")
        self.assertEqual(report["checks"][0]["error"], "not parity")


if __name__ == "__main__":
    unittest.main()
