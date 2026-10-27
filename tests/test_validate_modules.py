"""Stdlib unit tests for scoped graph validation; no Bazel executable required."""

import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    "validate_modules", Path(__file__).resolve().parents[1] / "scripts/validate_modules.py")
validation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validation)


def result(stdout="", stderr="", code=0, **extra):
    return {"id": 0, "name": "test", "stdout": stdout, "stderr": stderr,
            "returncode": code, "timed_out": False, **extra}


class FixtureTests(unittest.TestCase):
    def setUp(self):
        self.fixture = validation.load_fixtures()
        self.cases = {case["name"]: case for case in self.fixture["cases"]}

    def graph(self, name):
        return validation.expected_graph(self.cases[name], self.fixture)

    def test_case_count_and_required_scope(self):
        self.assertEqual(len(self.cases), 11)
        self.assertIn("discarded_version_raises_other_dep", self.cases)
        self.assertIn("nodep_fulfilled_upgrade_no_edge", self.cases)
        self.assertIn("multiple_version_snap_aliases", self.cases)

    def test_exact_root_and_expanded_fields(self):
        graph = self.graph("chain")
        self.assertEqual(set(graph), {"key", "name", "version", "apparentName", "root",
                                      "dependencies", "indirectDependencies", "cycles"})
        self.assertEqual(graph["key"], "<root>")
        a = graph["dependencies"][0]
        self.assertEqual(set(a), set(graph) - {"root"})
        self.assertEqual(a["dependencies"][0]["dependencies"], [])

    def test_bfs_unexpanded_has_no_children_fields(self):
        graph = self.graph("root_shared_bfs")
        shared = graph["dependencies"][0]["dependencies"][0]
        self.assertEqual(set(shared), {"key", "name", "version", "apparentName", "unexpanded"})
        self.assertIs(shared["unexpanded"], True)
        self.assertNotIn("unexpanded", graph["dependencies"][1])

    def test_override_key_differs_from_declared_version(self):
        node = self.graph("local_override_declared_version")["dependencies"][0]
        self.assertTrue(node["key"].endswith("@_"))
        self.assertEqual(node["version"], "9.7")
        self.assertEqual(node["apparentName"], "chosen_alias")

    def test_nodep_upgrade_does_not_add_root_edge(self):
        graph = self.graph("nodep_fulfilled_upgrade_no_edge")
        self.assertEqual(len(graph["dependencies"]), 1)
        self.assertEqual(graph["dependencies"][0]["dependencies"][0]["version"], "2.0")

    def test_mvo_aliases_and_two_versions(self):
        nodes = self.graph("multiple_version_snap_aliases")["dependencies"]
        self.assertEqual([n["dependencies"][0]["apparentName"] for n in nodes],
                         ["high_alias", "low_alias", "mid_alias"])
        self.assertEqual([n["dependencies"][0]["version"] for n in nodes], ["3.0", "2.0", "2.0"])
        self.assertIs(nodes[2]["dependencies"][0]["unexpanded"], True)

    def test_cycles_are_separate_and_hideable(self):
        cyclic = self.graph("graph_cycles")["dependencies"][0]["dependencies"][0]
        hidden = self.graph("graph_cycles_hidden")["dependencies"][0]["dependencies"][0]
        self.assertEqual(cyclic["dependencies"], [])
        self.assertEqual(len(cyclic["cycles"]), 1)
        self.assertEqual(hidden["cycles"], [])

    def test_private_local_path_registry_sources_and_metadata(self):
        original = validation.FIXTURES.read_bytes()
        with tempfile.TemporaryDirectory(dir=Path(tempfile.gettempdir()).resolve()) as temporary:
            scratch = Path(temporary)
            workspace, registry, override = validation.materialize(scratch, self.fixture)
            entries = list(registry.glob("modules/*/*/source.json"))
            self.assertEqual(len(entries), len(self.fixture["modules"]))
            for entry in entries:
                source = json.loads(entry.read_text())
                self.assertEqual(source["type"], "local_path")
                path = Path(source["path"])
                self.assertTrue(path.is_absolute())
                self.assertTrue(path.is_relative_to(scratch))
                self.assertEqual((path / "MODULE.bazel").read_text(), (entry.parent / "MODULE.bazel").read_text())
            self.assertFalse((registry / "modules/bazel_tools").exists())
            for case in self.cases.values():
                text = validation.root_text(case, self.fixture, override)
                self.assertNotIn("{ns}", text)
                self.assertNotIn("{override_path}", text)
                self.assertNotIn("bazel_tools", text)
            self.assertTrue(workspace.is_dir())
        self.assertFalse(scratch.exists())
        self.assertEqual(original, validation.FIXTURES.read_bytes())

    def test_generation_rejects_path_escape(self):
        self.fixture["modules"] = {"../escape@1.0": ""}
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            with self.assertRaisesRegex(ValueError, "unsafe"):
                validation.materialize(Path(temporary), self.fixture)


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        fixture = validation.load_fixtures()
        self.graph = validation.expected_graph(fixture["cases"][0], fixture)

    def test_parse_json_and_compare_exactly(self):
        parsed = validation.parse_graph(result(json.dumps(self.graph)))
        self.assertEqual(parsed, self.graph)
        validation.compare_graphs(parsed, copy.deepcopy(parsed), self.graph)

    def test_missing_fields_and_extra_root_deps_are_not_filtered(self):
        for mutation in ("field", "dep", "order"):
            changed = copy.deepcopy(self.graph)
            if mutation == "field":
                del changed["cycles"]
            elif mutation == "dep":
                changed["dependencies"].append({"key": "platforms@1.0"})
            else:
                changed["dependencies"][0]["dependencies"].insert(0, {"key": "extra@1.0"})
            with self.subTest(mutation=mutation), self.assertRaisesRegex(ValueError, "trees differ"):
                validation.compare_graphs(self.graph, changed, self.graph)

    def test_array_order_is_significant(self):
        first = {"dependencies": [{"key": "a"}, {"key": "b"}]}
        second = {"dependencies": list(reversed(first["dependencies"]))}
        with self.assertRaises(ValueError):
            validation.compare_graphs(first, second, first)

    def test_agreement_is_not_enough_without_expected_graph(self):
        with self.assertRaisesRegex(ValueError, "hand-authored"):
            validation.compare_graphs({}, {}, self.graph)

    def test_missing_failed_or_malformed_graph_is_not_success(self):
        for record in (result("{}"), result("[]"), result("not json"), result("", code=1),
                       result(json.dumps(self.graph), timed_out=True), result(json.dumps(self.graph), code=-11)):
            with self.subTest(record=record), self.assertRaises(ValueError):
                validation.parse_graph(record)

    def test_controlled_failure_requires_diagnostic_and_empty_stdout(self):
        validation.require_rejection(result(stderr="module not found", code=1), "not found")
        for record in (result(stderr="module not found"), result(stderr="module not found", code=None),
                       result(stderr="module not found", code=-11), result(stderr="module not found", code=139),
                       result(stderr="module not found", code=101), result(stderr="panicked: not found", code=1),
                       result(stderr="not found", code=1, timed_out=True), result(stderr="wrong error", code=1),
                       result("{}", "not found", code=1), result(stderr="not found", code=1, spawn_error="missing")):
            with self.subTest(record=record), self.assertRaises(ValueError):
                validation.require_rejection(record, "not found")


class InvocationTests(unittest.TestCase):
    def test_native_only_uses_supported_flags_and_local_registry(self):
        argv = validation.native_command(Path("/native"), Path("/private/registry space"), ["--nocycles"])
        self.assertEqual(argv[:4], ["/native", "--ignore_all_rc_files", "mod", "graph"])
        self.assertIn("--registry=file:///private/registry%20space", argv)
        self.assertNotIn("--remote_cache=", argv)
        self.assertFalse(any("https:" in arg for arg in argv))

    def test_reference_genuine_builtins_and_isolated_roots(self):
        runner = mock.Mock()
        ref = validation.support.Reference(Path("/official"), Path("/workspace"), Path("/scratch"), runner, "modules")
        validation.reference_graph(ref, "case", Path("/private/registry"))
        self.assertTrue(ref.started)
        argv = runner.run.call_args.args[1]
        self.assertEqual(argv[argv.index("mod"):argv.index("mod") + 2], ["mod", "graph"])
        for flag in ("--registry=file:///private/registry", "--registry=https://bcr.bazel.build",
                     "--noinclude_builtin", "--output_base=/scratch/modules-output-base",
                     "--remote_cache=", "--remote_executor=", "--bes_backend=", "--lockfile_mode=off"):
            self.assertIn(flag, argv)
        self.assertFalse(any("override" in arg for arg in argv))

    def test_reference_failure_is_not_differential_pass(self):
        fixture = validation.load_fixtures()
        runner = mock.Mock(report={"checks": []})
        runner.run.return_value = result("{}")
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            ref = mock.Mock(workspace=Path(temporary))
            with mock.patch.object(validation, "reference_graph", return_value=result(stderr="network unavailable", code=1)):
                with self.assertRaises(validation.ReferenceFailure):
                    validation.differential(fixture["cases"][0], fixture, Path(temporary), Path(temporary),
                                            Path("/native"), ref, runner)
        self.assertEqual(runner.report["checks"][0]["status"], "REFERENCE_FAILURE")

    def test_main_missing_executable_writes_failure_artifact(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            output = Path(temporary) / "report.json"
            code = validation.main(["--native", temporary + "/missing", "--output", str(output)])
            report = json.loads(output.read_text())
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "FAIL")
        self.assertEqual(report["runs"], [])
        self.assertTrue(report["errors"])

    def test_missing_fixture_is_not_an_empty_successful_suite(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            missing = Path(temporary) / "missing.json"
            with self.assertRaises(FileNotFoundError):
                validation.load_fixtures(missing)

    def test_main_cleans_private_scratch_after_reference_failure(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            output = Path(temporary) / "report.json"
            reference = mock.Mock()
            reference.startup = ["/official", "--ignore_all_rc_files"]
            with mock.patch.object(validation.support, "validate_executable"), \
                    mock.patch.object(validation.support, "file_evidence", return_value={"sha256": "test"}), \
                    mock.patch.object(validation.support.Runner, "run", return_value=result("bazel 9.3.0\n")), \
                    mock.patch.object(validation.support, "Reference", return_value=reference), \
                    mock.patch.object(validation, "differential", side_effect=validation.ReferenceFailure("external failure")):
                reference.started = True
                code = validation.main(["--output", str(output)])
            report = json.loads(output.read_text())
        reference.shutdown.assert_called_once()
        self.assertEqual(code, 1)
        self.assertTrue(report["scratch_cleaned"])
        self.assertTrue(report["reference_shutdown_attempted"])
        self.assertFalse(Path(report["scratch_directory"]).exists())
        self.assertEqual(report["completed_differential_cases"], 0)


if __name__ == "__main__":
    unittest.main()
