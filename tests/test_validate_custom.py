"""Fixture/helper/mock tests only: never launch native or reference Bazel."""

import argparse
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
import validate_custom as custom


def record(stdout="", stderr="", returncode=0, timed_out=False):
    return {"id": 0, "name": "mock", "stdout": stdout, "stderr": stderr,
            "returncode": returncode, "timed_out": timed_out}


class FixtureTests(unittest.TestCase):
    def test_root_inventory_matches_declared_rules(self):
        text = (custom.FIXTURE / "BUILD.bazel").read_text()
        names = re.findall(r'^\w+\(name = "([^"]+)"', text, re.M)
        self.assertEqual(sorted("//:" + name for name in names), custom.ROOT_LABELS)
        self.assertEqual(len(names), len(set(names)))
        self.assertTrue((custom.FIXTURE / "MODULE.bazel").is_file())

    def test_error_inventory_is_isolated_and_complete(self):
        categories = {case[0] for case in custom.ERROR_CASES}
        self.assertEqual(categories, {"mandatory", "type", "unknown", "implementation",
                                     "unproduced", "duplicate", "source", "single",
                                     "foreign", "cycle", "visibility", "none-label", "none-output"})
        for category, command, target, pattern in custom.ERROR_CASES:
            package, name = target[2:].split(":")
            self.assertIn('name = "' + name + '"', (custom.FIXTURE / package / "BUILD.bazel").read_text())
            self.assertEqual(command, "query" if category in ("mandatory", "type", "unknown", "none-label", "none-output") else "build")
            re.compile(pattern)

    def test_expected_bytes_cardinalities_and_actions(self):
        self.assertEqual((custom.FIXTURE / "a.txt").read_bytes(), b"alpha\n")
        self.assertEqual((custom.FIXTURE / "b.txt").read_bytes(), b"bravo\n")
        cases = {target: (outputs, count) for target, outputs, count in custom.OUTPUT_CASES}
        self.assertEqual(cases["//:scalar"][0]["scalar.txt"].decode(), "@@//:scalar|héllo 雪|first|7|True\n")
        self.assertEqual(cases["//:snapshot"][0]["snapshot.txt"], b"@@//:snapshot|override|caller|-2|False\n")
        self.assertEqual(len(cases["//:forwarded"][0]), 2)
        self.assertEqual(cases["//:forwarded"][1], 0)
        self.assertEqual(len(cases["//:inspected"][0]), 3)
        self.assertEqual(cases["//:inspected"][1], 3)
        self.assertEqual(cases["//:inspect-extra.txt"][1], 1)
        self.assertEqual(custom.INSPECT_BYTES, b"b.txt|a.txt\nb.txt|a.txt\na.txt|a.txt\n")
        self.assertEqual(custom.chain_expected()["chain-final.txt"], b"alpha\nseed\nargument value|env value\n")

    def test_fixture_api_and_forward_action_registration(self):
        text = (custom.FIXTURE / "defs.bzl").read_text()
        for api in ("attr.string(", "attr.string_list(", "attr.int(", "attr.bool(",
                    "attr.label(", "attr.label_list(", "attr.output(", "attr.output_list(",
                    "ctx.attr.", "ctx.files.", "ctx.file.", "ctx.outputs.", "ctx.label",
                    "ctx.actions.declare_file(", "ctx.actions.write(", "ctx.actions.run_shell(",
                    "[DefaultInfo].files", '.short_path', '.basename', 'order = "postorder"'):
            self.assertIn(api, text)
        self.assertLess(text.index("ctx.actions.run_shell("), text.index('ctx.actions.write(seed,'))
        self.assertIn("false; cat", text)
        self.assertNotIn("set -e", text)
        self.assertIn("return DefaultInfo(files = depset([out]))", text)
        self.assertIn("return [DefaultInfo(files = depset([out]))]", text)

    def test_advanced_inventory_and_exact_reference_verified_bytes(self):
        self.assertEqual(len(custom.ADVANCED_OUTPUT_CASES), 9)
        for target, outputs, count in custom.ADVANCED_OUTPUT_CASES:
            package, name = target[2:].split(":")
            build = (custom.FIXTURE / package / "BUILD.bazel").read_text()
            self.assertIn('name = "' + name + '"', build)
            self.assertEqual(len(outputs), 2 if package.endswith("args") else 1)
            self.assertEqual(count, 1 if package.endswith(("label_caller", "none")) else 2)
        cases = {target: outputs for target, outputs, _ in custom.ADVANCED_OUTPUT_CASES}
        self.assertEqual(cases["//advanced/consumer:consumed"]["consumed.txt"],
                         b"provider payload\nprovider message|one,two|nested|5\n")
        self.assertEqual(len(custom.DIRECT_BYTES), 97)
        self.assertEqual(hashlib.sha256(custom.DIRECT_BYTES).hexdigest(),
                         "daa6b86d8219761db8bbf5e739bf3209ca4b5f7f9f4593f5409cdee0acb45125")
        self.assertIn(b"literal $HOME; *.txt 'quoted' \"double\"\n", custom.DIRECT_BYTES)
        definition = b"advanced/label_helper/source.txt"
        self.assertEqual(cases["//advanced/label_caller:defaults"]["defaults.txt"], definition + b"|" + definition + b"\n")
        self.assertEqual(cases["//advanced/label_caller:immutable_label"]["immutable_label.txt"], definition + b"|" + definition + b"\n")
        self.assertEqual(cases["//advanced/label_caller:caller_string"]["caller_string.txt"],
                         b"advanced/label_caller/source.txt|" + definition + b"\n")

    def test_provider_payload_is_nondefault_and_loaded_independently(self):
        producer = (custom.FIXTURE / "advanced/producer/defs.bzl").read_text()
        consumer = (custom.FIXTURE / "advanced/consumer/defs.bzl").read_text()
        for text in (producer, consumer):
            self.assertIn('load("//advanced/provider_defs:defs.bzl"', text)
        self.assertIn('ProducerPayload = "PayloadInfo"', producer)
        self.assertIn('ConsumerPayload = "PayloadInfo"', consumer)
        self.assertIn("DefaultInfo(files = depset([]))", producer)
        self.assertIn("metadata = struct(tag =", producer)
        self.assertIn("ConsumerPayload not in dep or OtherInfo in dep", consumer)
        self.assertIn("inputs = [payload.file]", consumer)
        self.assertEqual(custom.ADVANCED_EMPTY_CASES, (("//advanced/producer:payload", "provider-only.txt"),))

    def test_direct_actions_use_file_and_string_executables_tools_and_sibling(self):
        text = (custom.FIXTURE / "advanced/direct/defs.bzl").read_text()
        for fragment in ("is_executable = True", "sibling = script", "executable = script",
                         'executable = "/bin/bash"', "tools = [script, ctx.file.tool]",
                         "tools = [ctx.file.tool]", "env = env"):
            self.assertIn(fragment, text)
        self.assertEqual(custom.ADVANCED_PATH_SUFFIXES, {
            "//advanced/direct:file_exec": "advanced/direct/file_exec/file_exec.txt",
            "//advanced/direct:string_exec": "advanced/direct/string_exec/string_exec.txt",
        })

    def test_advanced_error_inventory_excludes_unsupported_provider_init(self):
        self.assertEqual({case[0] for case in custom.ADVANCED_ERROR_CASES},
                         {"provider-fields", "provider-missing", "provider-duplicate", "provider-frozen"})
        for _, command, target, pattern in custom.ADVANCED_ERROR_CASES:
            package, name = target[2:].split(":")
            self.assertIn('name = "' + name + '"', (custom.FIXTURE / package / "BUILD.bazel").read_text())
            self.assertEqual(command, "build")
            re.compile(pattern)


class ParserAndInvocationTests(unittest.TestCase):
    def test_defaults_and_no_repository_option(self):
        args = custom.argument_parser().parse_args([])
        self.assertEqual(args.timeout, 90)
        self.assertEqual(args.output.name, "custom-validation.json")
        self.assertFalse(hasattr(args, "repo"))

    def test_timeout_validation(self):
        for text in ("0", "-1", "nan", "inf", "invalid"):
            with self.subTest(text=text), self.assertRaises(argparse.ArgumentTypeError):
                custom.positive_seconds(text)

    def test_native_command_is_direct_and_jobs_only_for_build(self):
        runner = mock.Mock()
        runner.run.return_value = record()
        custom.native_invoke(Path("/native"), Path("/cache"), runner, Path("/fixture"), "b", "build", "//:scalar", 4)
        self.assertEqual(runner.run.call_args.args[1], [
            "/native", "--ignore_all_rc_files", "--cache-dir=/cache", "build", "--lockfile_mode=off", "--jobs=4",
            "//:scalar"])
        custom.native_invoke(Path("/native"), Path("/cache"), runner, Path("/fixture"), "q", "query", "//:all", 4)
        self.assertFalse(any(arg.startswith("--jobs") for arg in runner.run.call_args.args[1]))

    def test_reported_fallback_is_rejected(self):
        for diagnostic in ("falling back to bazel", "fallback to reference", "fallback enabled"):
            runner = mock.Mock()
            runner.run.return_value = record(stderr=diagnostic)
            with self.assertRaisesRegex(ValueError, "fallback"):
                custom.native_invoke(Path("/native"), Path("/cache"), runner, Path("/fixture"), "q", "query", "//:all")


class EvidenceTests(unittest.TestCase):
    def test_error_fixture_path_alone_is_not_a_category_diagnostic(self):
        for category, _, _, pattern in custom.ERROR_CASES + custom.ADVANCED_ERROR_CASES:
            with self.subTest(category=category), self.assertRaises(ValueError):
                custom.expected_error(record(
                    stderr="unsupported custom API at /errors/" + category + "/BUILD.bazel",
                    returncode=1), pattern)

    def test_error_checker_requires_category_and_real_failure(self):
        accepted = record(stderr="custom implementation deliberate failure", returncode=1)
        pattern = next(case[3] for case in custom.ERROR_CASES if case[0] == "implementation")
        self.assertEqual(custom.expected_error(accepted, pattern)["status"], "EXPECTED_FAILURE")
        for change in ({"returncode": 0}, {"returncode": None}, {"returncode": -9},
                       {"timed_out": True}, {"stderr": "unrelated shell failure"},
                       {"stdout": "//:unexpected\n"}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                custom.expected_error(dict(accepted, **change), pattern)

    def test_error_parity_never_accepts_one_sided_failure(self):
        case = next(case for case in custom.ERROR_CASES if case[0] == "implementation")
        failure = record(stderr="custom implementation deliberate failure", returncode=1)
        reference = mock.Mock(workspace=Path("/fixture"))
        for native_result, reference_result in ((failure, record()), (record(), failure)):
            reference.run.return_value = reference_result
            with mock.patch.object(custom, "native_invoke", return_value=native_result), self.assertRaises(ValueError):
                custom.error_parity(Path("/native"), reference, Path("/cache"), mock.Mock(), case)
        reference.run.return_value = failure
        with mock.patch.object(custom, "native_invoke", return_value=failure):
            result = custom.error_parity(Path("/native"), reference, Path("/cache"), mock.Mock(), case)
        self.assertEqual(result["native"]["status"], result["reference"]["status"])

    def test_query_requires_both_exact_label_sets(self):
        target = "//errors/implementation:bad"
        reference = mock.Mock(workspace=Path("/fixture"))
        reference.run.return_value = record(target + "\n")
        with mock.patch.object(custom, "native_invoke", return_value=record(target + "\n")):
            self.assertEqual(custom.query_parity(Path("/native"), reference, Path("/cache"), mock.Mock(), target, [target])[
                "labels"]["native"], [target])
        with mock.patch.object(custom, "native_invoke", return_value=record("//:wrong\n")), self.assertRaises(ValueError):
            custom.query_parity(Path("/native"), reference, Path("/cache"), mock.Mock(), target, [target])

    def test_known_foreign_reference_crash_is_not_claimed_as_diagnostic_parity(self):
        case = next(case for case in custom.ERROR_CASES if case[0] == "foreign")
        reference = mock.Mock(workspace=Path("/fixture"))
        reference.run.return_value = record(
            stderr="FATAL: bazel crashed due to an internal error\nSource in outputs: File: a.txt", returncode=37)
        with mock.patch.object(custom, "native_invoke", return_value=record(
                stderr="cannot write source file", returncode=1)):
            result = custom.error_parity(Path("/native"), reference, Path("/cache"), mock.Mock(), case)
        self.assertFalse(result["diagnostic_parity"])
        self.assertEqual(result["reference_failure_kind"], "KNOWN_REFERENCE_INTERNAL_CRASH")
        self.assertIn("failure outcome only", result["comparison"])

    def test_unrelated_or_native_crashes_never_count_as_expected_failures(self):
        case = next(case for case in custom.ERROR_CASES if case[0] == "foreign")
        reference = mock.Mock(workspace=Path("/fixture"))
        reference.run.return_value = record(stderr="FATAL: bazel crashed: source file", returncode=37)
        with mock.patch.object(custom, "native_invoke", return_value=record(
                stderr="cannot write source file", returncode=1)), self.assertRaisesRegex(ValueError, "unexpected reference crash"):
            custom.error_parity(Path("/native"), reference, Path("/cache"), mock.Mock(), case)
        reference.run.return_value = record(stderr="cannot write source file", returncode=1)
        with mock.patch.object(custom, "native_invoke", return_value=record(
                stderr="panicked at source file", returncode=101)), self.assertRaisesRegex(ValueError, "native crashed"):
            custom.error_parity(Path("/native"), reference, Path("/cache"), mock.Mock(), case)

    def test_exact_bytes_hashes_sizes_and_output_cardinality(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            path = Path(directory) / "unicode.txt"
            data = "héllo 雪\n".encode()
            path.write_bytes(data)
            result = custom.fixture_outputs([path], {path.name: data})[0]
            self.assertEqual(result["size_bytes"], len(data))
            self.assertEqual(result["sha256"], hashlib.sha256(data).hexdigest())
            for paths, expected in (([], {path.name: data}), ([path], {}),
                                    ([path, path], {path.name: data}), ([path], {path.name: b"wrong"})):
                with self.assertRaises(ValueError):
                    custom.fixture_outputs(paths, expected)
            self.assertEqual(custom.fixture_outputs([], {}), [])

    def test_action_counts_are_measured_not_assumed(self):
        self.assertEqual(custom.action_counts(record(stderr="4 actions executed, 0 cache hits"), (4, 0)),
                         {"actions_executed": 4, "cache_hits": 0})
        for result in (record(), record(stderr="3 actions executed, 1 cache hits"),
                       record(stderr="4 actions executed, 0 cache hits", timed_out=True)):
            with self.assertRaises(ValueError):
                custom.action_counts(result, (4, 0))

    def test_build_pair_uses_reference_cquery_and_verifies_artifacts(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            path = root / "out.txt"
            path.write_bytes(b"result\n")
            reference = mock.Mock(workspace=root)
            reference.run.return_value = record()
            result = record("//:out\t" + str(path), "1 actions executed, 0 cache hits")
            with mock.patch.object(custom, "native_invoke", return_value=result), \
                    mock.patch.object(custom, "output_listing", return_value=[path]) as listing:
                details = custom.build_pair(Path("/native"), reference, root / "cache", mock.Mock(),
                                            "//:out", {"out.txt": b"result\n"}, (1, 0), "case", 4)
            listing.assert_called_once_with(reference, "//:out")
            reference.run.assert_called_once_with("case.reference", "build", "--jobs=4", "//:out")
            self.assertEqual(details["native"], details["reference"])

    def test_empty_case_checks_absence_not_just_empty_listing(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            cache = root / "cache"
            cache.mkdir()
            reference = mock.Mock(output_base=root / "ref")
            with mock.patch.object(custom, "build_pair", return_value={}):
                self.assertIn("unselected_output_absent", custom.empty_case(
                    Path("/native"), reference, cache, mock.Mock(), custom.EMPTY_CASES[0]))
                (cache / "unused-dynamic.txt").write_text("incorrectly executed")
                with self.assertRaisesRegex(ValueError, "unselected action"):
                    custom.empty_case(Path("/native"), reference, cache, mock.Mock(), custom.EMPTY_CASES[0])

    def test_output_case_checks_cold_and_warm(self):
        with mock.patch.object(custom, "build_pair", return_value={}) as pair:
            custom.output_case(Path("/native"), mock.Mock(), Path("/cache"), mock.Mock(), custom.OUTPUT_CASES[3])
        self.assertEqual([call.args[6] for call in pair.call_args_list], [(3, 0), (0, 3)])

    def test_sibling_path_validation_allows_configuration_prefix_difference(self):
        case = next(case for case in custom.ADVANCED_OUTPUT_CASES if case[0] == "//advanced/direct:file_exec")
        suffix = custom.ADVANCED_PATH_SUFFIXES[case[0]]
        evidence = {
            "native": [{"path": "/cache/bazel-out/rust-fastbuild/bin/" + suffix}],
            "reference": [{"path": "/ref/bazel-out/k8-fastbuild/bin/" + suffix}],
        }
        with mock.patch.object(custom, "build_pair", return_value=evidence) as pair:
            details = custom.output_case(Path("/native"), mock.Mock(), Path("/cache"), mock.Mock(), case)
        self.assertEqual([call.args[6] for call in pair.call_args_list], [(2, 0), (0, 2)])
        self.assertTrue(all(build["sibling_path_suffix"] == suffix for build in details))

    def test_sibling_path_validation_rejects_wrong_directory_even_with_right_basename(self):
        case = next(case for case in custom.ADVANCED_OUTPUT_CASES if case[0] == "//advanced/direct:file_exec")
        evidence = {
            "native": [{"path": "/cache/bazel-out/rust-fastbuild/bin/advanced/direct/file_exec.txt"}],
            "reference": [{"path": "/ref/bazel-out/k8-fastbuild/bin/advanced/direct/file_exec/file_exec.txt"}],
        }
        with mock.patch.object(custom, "build_pair", return_value=evidence), self.assertRaisesRegex(ValueError, "sibling path"):
            custom.output_case(Path("/native"), mock.Mock(), Path("/cache"), mock.Mock(), case)


class SourceChangeTests(unittest.TestCase):
    def test_chain_separate_caches_counts_source_change_and_restore(self):
        tracked = (custom.FIXTURE / "a.txt").read_bytes()
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            fixture = root / "fixture"
            shutil.copytree(custom.FIXTURE, fixture)
            reference = mock.Mock(workspace=fixture)
            seen = []

            def pair(native, ref, cache, runner, target, expected, counts, name, jobs):
                seen.append((cache, counts, jobs, (fixture / "a.txt").read_bytes()))
                self.assertEqual(expected, custom.chain_expected(seen[-1][-1]))
                return {}

            with mock.patch.object(custom, "build_pair", side_effect=pair):
                self.assertEqual(len(custom.chain_builds(Path("/native"), reference, root, mock.Mock())), 6)
            self.assertEqual([item[1] for item in seen], [(4, 0), (0, 4), (4, 0), (0, 4), (2, 2), (2, 2)])
            self.assertEqual([item[2] for item in seen], [1, 1, 4, 4, 1, 4])
            self.assertEqual(seen[0][0], seen[1][0])
            self.assertNotEqual(seen[0][0], seen[2][0])
            self.assertEqual(seen[-1][-1], b"changed alpha\n")
            self.assertEqual((fixture / "a.txt").read_bytes(), tracked)
        self.assertEqual((custom.FIXTURE / "a.txt").read_bytes(), tracked)

    def test_source_restored_on_interruption(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            fixture = root / "fixture"
            shutil.copytree(custom.FIXTURE, fixture)
            reference = mock.Mock(workspace=fixture)
            with mock.patch.object(custom, "build_pair", side_effect=[{}, {}, {}, {}, KeyboardInterrupt]), \
                    self.assertRaises(KeyboardInterrupt):
                custom.chain_builds(Path("/native"), reference, root, mock.Mock())
            self.assertEqual((fixture / "a.txt").read_bytes(), b"alpha\n")

    def test_tracked_fixture_cannot_be_mutated(self):
        reference = mock.Mock(workspace=custom.FIXTURE)
        with mock.patch.object(custom, "build_pair", return_value={}), \
                self.assertRaisesRegex(ValueError, "private fixture copy"):
            custom.chain_builds(Path("/native"), reference, Path("/tmp/opencode"), mock.Mock())


class LifecycleTests(unittest.TestCase):
    def run_mock_main(self, output, release="release 9.3.0\n", shutdown_error=None, interrupt=False):
        reference = mock.Mock()
        reference.run.return_value = record(release)
        reference.shutdown.side_effect = shutdown_error
        with mock.patch.object(custom, "validate_executable"), \
                mock.patch.object(custom, "file_evidence", return_value={}), \
                mock.patch.object(custom, "Reference", return_value=reference), \
                mock.patch.object(custom, "query_parity", side_effect=KeyboardInterrupt if interrupt else None, return_value={}), \
                mock.patch.object(custom, "chain_builds", return_value={}), \
                mock.patch.object(custom, "output_case", return_value={}), \
                mock.patch.object(custom, "empty_case", return_value={}), \
                mock.patch.object(custom, "error_parity", return_value={}), \
                mock.patch.object(custom.Runner, "run", side_effect=AssertionError("must never launch Bazel")), \
                mock.patch("builtins.print"):
            code = custom.main(["--output", str(output)])
        reference.shutdown.assert_called_once()
        report = json.loads(output.read_text())
        self.assertTrue(report["scratch_cleaned"])
        self.assertFalse(Path(report["scratch_directory"]).exists())
        self.assertIn("cache_sizes_bytes_before_cleanup", report)
        return code, report

    def test_wrong_reference_release_fails_and_cleans(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            code, report = self.run_mock_main(Path(directory) / "report.json", release="release 9.1.1\n")
        self.assertEqual(code, 1)
        self.assertIn("reference binary is not official release 9.3.0", report["errors"])

    def test_all_scoped_checks_are_required_for_success(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            code, report = self.run_mock_main(Path(directory) / "report.json")
        self.assertEqual(code, 0)
        self.assertEqual(report["status"], "PASS")
        self.assertEqual(len(report["checks"]), custom.expected_check_count())
        names = [item["name"] for item in report["checks"]]
        self.assertEqual(len(names), len(set(names)))
        self.assertLess(names.index("fixture.empty.//advanced/producer:payload"),
                        names.index("fixture.output.//advanced/consumer:consumed"))
        for target, _, _ in custom.ADVANCED_OUTPUT_CASES:
            self.assertIn("fixture.output." + target, names)
        for category, _, _, _ in custom.ADVANCED_ERROR_CASES:
            self.assertIn("fixture.error." + category, names)

    def test_shutdown_error_is_reported_and_scratch_still_removed(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            code, report = self.run_mock_main(Path(directory) / "report.json", shutdown_error=ValueError("shutdown failed"))
        self.assertEqual(code, 1)
        self.assertIn("shutdown: shutdown failed", report["errors"])

    def test_interrupt_shuts_down_and_cleans(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            code, report = self.run_mock_main(Path(directory) / "report.json", interrupt=True)
        self.assertEqual(code, 130)
        self.assertIn("interrupted", report["errors"])

    def test_report_cannot_modify_tracked_fixture(self):
        with self.assertRaisesRegex(ValueError, "outside tracked fixtures"):
            custom.main(["--output", str(custom.FIXTURE / "report.json")])


if __name__ == "__main__":
    unittest.main()
