"""Stdlib harness tests: no native/reference Bazel subprocess is launched."""

import argparse
import base64
import copy
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock
from urllib.parse import unquote, urlparse

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import validate_direct_repositories as direct


def record(stdout="", stderr="", returncode=0, **extra):
    return {"id": 0, "name": "mock", "stdout": stdout, "stderr": stderr,
            "returncode": returncode, "timed_out": False, **extra}


MESSAGES = {
    "declaration_signature": "use_repo_rule() requires two positional string arguments",
    "call_signature": "repository rule invocation accepts no positional arguments",
    "name_required": "missing required keyword argument: name",
    "name_type": "name must be a string",
    "dev_type": "dev_dependency must be a bool",
    "invalid_name": "invalid user-provided repo name '_x': valid names must start with a letter",
    "collision": "The repo name 'generated' is already being used by a bazel_dep call",
    "module_order": "module() must be called before any other functions",
    "syntax_kwargs": "**kwargs must be a dict literal in MODULE.bazel",
    "syntax_star": "*args are not allowed in MODULE.bazel",
    "syntax_rebinding": 'global binding "x" may not be reassigned; first bound at MODULE.bazel:3:1',
    "ordinary_extension_unsupported": "Variable `module_extension` not found",
    "declaration_loaded": "Error in fail: declaration fixture was loaded",
}


# Verbatim diagnostic lines from the first release run in
# artifacts/direct-repositories-validation.json; no runtime artifact dependency.
# Keep native and official spellings: neither engine uses the old mock wording.
RECORDED_DECLARATION_TYPES = (
    (1, "error: bzl_file must be a string"),
    (1, "error: rule_name must be a string"),
    (48, "Error in use_repo_rule: in call to use_repo_rule(), parameter 'repo_rule_bzl_file' got value of type 'int', want 'string'"),
    (48, "Error in use_repo_rule: in call to use_repo_rule(), parameter 'repo_rule_name' got value of type 'int', want 'string'"),
)
RECORDED_COLLISIONS = (
    (1, 'error: repository name "m" is already used'),
    (1, 'error: repository name "dep" is already used'),
    (1, 'error: repository name "reserved" is already used'),
    (1, 'error: repository name "generated" is already used'),
    (48, "Error in call: The repo name 'm' cannot be defined by a repo rule at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:4:5 as it is already defined as the current module name at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:1:7"),
    (48, "Error in call: The repo name 'dep' cannot be defined by a repo rule at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:4:5 as it is already defined by a bazel_dep at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:2:10"),
    (48, "Error in call: The repo name 'reserved' cannot be defined by a repo rule at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:6:5 as it is already defined by a use_repo() call at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:4:9"),
    (48, "Error in call: The repo name 'reserved' cannot be defined by a repo rule at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:5:5 as it is already defined by a bazel_dep at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:3:10"),
    (48, "Error in call: The repo name 'generated' cannot be defined by a repo rule at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:5:5 as it is already defined by a repo rule at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:4:5"),
    (48, "Error in call: The repo name 'generated' cannot be defined by a repo rule at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:6:6 as it is already defined by a repo rule at /tmp/opencode/direct-repositories-hhwllx6h/workspace/MODULE.bazel:5:5"),
)

# Exact debug CLI output retained in artifacts/direct-declaration-boundary-update.json.
# The full file exports a supported repository_rule AND an unsupported module_extension.
RECORDED_ORDINARY_EXTENSION = (
    "bazel-rs: cannot evaluate //:defs.bzl: error: Variable `module_extension` not found\n"
    " --> //:defs.bzl:7:7\n"
    "  |\n"
    "7 | ext = module_extension(implementation = _ext_impl)\n"
    "  |       ^^^^^^^^^^^^^^^^\n"
    "  |\n\n"
)


class RecordedDiagnosticTests(unittest.TestCase):
    def test_actual_ordinary_extension_constructor_absence(self):
        result = record(stdout="", stderr=RECORDED_ORDINARY_EXTENSION, returncode=1)
        self.assertEqual(direct.require_rejection(result, "ordinary_extension_unsupported"),
                         {"diagnostic_class": "ordinary_extension_unsupported", "returncode": 1})

    def test_ordinary_extension_boundary_rejects_unrelated_failures(self):
        good = record(stderr=RECORDED_ORDINARY_EXTENSION, returncode=1)
        for stderr in (
                "network unavailable", "permission denied", "syntax error", "module_extension",
                "Variable `repository_rule` not found", "Variable `other` not found\nmodule_extension()",
                "module_extension source file not found", "repository rule execution is not implemented",
                "module_extension(name = ) syntax error",
                *(RECORDED_ORDINARY_EXTENSION + suffix for suffix in (
                    "network unavailable", "permission denied", "syntax error", "parse error",
                    "unexpected token", "panicked", "internal error", "FATAL:", "crashed", "falling back"))):
            with self.subTest(stderr=stderr), self.assertRaises(ValueError):
                direct.require_rejection(dict(good, stderr=stderr), "ordinary_extension_unsupported")
        for change in ({"returncode": 0}, {"returncode": None}, {"returncode": -9}, {"returncode": 101},
                       {"returncode": 139}, {"returncode": True}, {"timed_out": True},
                       {"interrupted": True}, {"spawn_error": "absent"}, {"stdout": "@@generated//:data\n"}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                direct.require_rejection(dict(good, **change), "ordinary_extension_unsupported")

    def test_ordinary_extension_boundary_matches_only_builtin_absence(self):
        for stderr in ("name 'module_extension' is not defined", "module_extension is unsupported",
                       "module_extension constructor is not supported"):
            with self.subTest(stderr=stderr):
                direct.require_rejection(record(stderr=stderr, returncode=1), "ordinary_extension_unsupported")

    def test_actual_native_and_reference_declaration_type_errors(self):
        for code, stderr in RECORDED_DECLARATION_TYPES:
            with self.subTest(stderr=stderr):
                direct.require_rejection(record(stderr=stderr, returncode=code), "declaration_signature")

    def test_actual_native_and_reference_collision_errors(self):
        for code, stderr in RECORDED_COLLISIONS:
            with self.subTest(stderr=stderr):
                direct.require_rejection(record(stderr=stderr, returncode=code), "collision")

    def test_actual_errors_do_not_match_wrong_class_or_unrelated_failures(self):
        for diagnostic, examples, wrong in (("declaration_signature", RECORDED_DECLARATION_TYPES, "collision"),
                                            ("collision", RECORDED_COLLISIONS, "declaration_signature")):
            for code, stderr in examples:
                with self.subTest(diagnostic=diagnostic, stderr=stderr):
                    with self.assertRaises(ValueError):
                        direct.require_rejection(record(stderr=stderr, returncode=code), wrong)
                    for prefix in ("syntax error: ", "unexpected token: ", "panicked: "):
                        with self.assertRaises(ValueError):
                            direct.require_rejection(record(stderr=prefix + stderr, returncode=code), diagnostic)
            for unrelated in ("error: foo must be a string", "error: the output path is already defined",
                              "ERROR: error executing MODULE.bazel file for <root>"):
                with self.subTest(unrelated=unrelated), self.assertRaises(ValueError):
                    direct.require_rejection(record(stderr=unrelated, returncode=1), diagnostic)


class FixtureTests(unittest.TestCase):
    def test_inventory_has_distinct_inputs_and_only_two_nonparity_probes(self):
        self.assertEqual(len(direct.CASES), 48)
        self.assertEqual(len(set(name for name, _ in direct.PLAN)), 48)
        self.assertEqual([(name, kind) for name, kind in direct.PLAN if kind != "differential"], [
            ("active-unused.load-probe", "reference_laziness_probe"),
            ("native.ordinary-extension-definition-boundary", "native_capability_boundary")])
        self.assertTrue(all(item["target"] == "@dep//:data" for item in direct.CASES[:-1]))
        self.assertEqual(direct.CASES[-1]["target"], "@generated//:data")
        self.assertEqual(direct.CASES[-1]["diagnostic_class"], "ordinary_extension_unsupported")
        self.assertEqual(direct.CASES[-1]["defs"], direct.REAL_DEFS)
        self.assertEqual(direct.CANONICAL, "@@decldep+//:data")

    def test_private_registry_uses_exact_case_modules_and_real_rule_definition(self):
        chosen = next(c for c in direct.CASES if c["name"] == "ignored.dependency")
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            root = Path(temporary)
            registry, files = direct.materialize(root / "case", root / "workspace", chosen)
            entry = registry / "modules/decldep/1.0"
            descriptor = json.loads((entry / "source.json").read_text())
            self.assertEqual(descriptor["type"], "local_path")
            source = Path(descriptor["path"])
            self.assertTrue(source.is_absolute() and source.is_relative_to(root))
            self.assertEqual((source / "MODULE.bazel").read_text(), chosen["dependency_module"])
            self.assertEqual((entry / "MODULE.bazel").read_text(), chosen["dependency_module"])
            self.assertEqual((root / "workspace/MODULE.bazel").read_text(), chosen["root_module"])
            self.assertEqual((source / "data").read_bytes(), b"direct repository declaration fixture\x00\xff\n")
            self.assertEqual((source / "BUILD.bazel").read_text(), 'exports_files(["data"])\n')
            self.assertFalse((registry / "modules/bazel_tools").exists())
            self.assertFalse((source / "missing.bzl").exists())
            self.assertTrue(all(path.read_bytes() == data for path, data in files.items()))
            self.assertNotIn("local_path_override", (root / "workspace/MODULE.bazel").read_text())
        # Syntax-check the trusted fixture text without executing a repository.
        compile(direct.REAL_DEFS, "defs.bzl", "exec")
        self.assertIn("repository_rule(implementation = _repo_impl)", direct.REAL_DEFS)
        self.assertIn("module_extension(implementation = _ext_impl)", direct.REAL_DEFS)
        self.assertNotIn("fail(", direct.REAL_DEFS)

    def test_snapshot_records_paths_exact_binary_bytes_hashes_and_mutations(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            path = Path(temporary) / "data"
            path.write_bytes(direct.DATA)
            before = direct.snapshot([path])
            evidence = before[str(path)]
            self.assertEqual(evidence["path"], str(path))
            self.assertEqual(evidence["sha256"], hashlib.sha256(direct.DATA).hexdigest())
            self.assertEqual(evidence["size_bytes"], len(direct.DATA))
            self.assertEqual(base64.b64decode(evidence["bytes_base64"]), direct.DATA)
            path.write_bytes(b"changed")
            self.assertNotEqual(direct.snapshot([path]), before)


class ComparisonTests(unittest.TestCase):
    def test_query_compares_single_repository_identity_not_equal_raw_spelling(self):
        native = direct.require_query(record("@@decldep+//:data\n"), native=True)
        official = direct.require_query(record("@dep//:data\n"))
        self.assertEqual(native["canonical_identity"], official["canonical_identity"])
        self.assertNotEqual(native["exact_labels"], official["exact_labels"])
        for text in ("//:data", "@@dep+//:data", "@other//:data", "@dep//:data\n@dep//:data", "", "not a label"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                direct.require_query(record(text))
        with self.assertRaises(ValueError):
            direct.require_query(record("@dep//:data\n"), native=True)

    def test_all_diagnostic_classes_have_semantic_not_exact_matches(self):
        for diagnostic, message in MESSAGES.items():
            with self.subTest(diagnostic=diagnostic):
                direct.require_rejection(record(stderr="ERROR: " + message, returncode=48), diagnostic)
        direct.require_rejection(record(stderr="duplicate repository name generated", returncode=1), "collision")
        direct.require_rejection(record(stderr="repo name 'm' already used", returncode=2), "collision")

    def test_rejection_refuses_crashes_timeouts_missing_class_and_parser_failures(self):
        good = record(stderr=MESSAGES["collision"], returncode=1)
        for change in ({"returncode": 0}, {"returncode": None}, {"returncode": -9}, {"returncode": 101},
                       {"returncode": 139}, {"returncode": True}, {"timed_out": True}, {"interrupted": True},
                       {"spawn_error": "absent"}, {"stdout": "@dep//:data"}, {"stderr": "network unavailable"},
                       {"stderr": "FATAL: " + MESSAGES["collision"]},
                       {"stderr": "syntax error at repo(name = 'duplicate')"},
                       {"stderr": "parse error: " + MESSAGES["collision"]},
                       {"stderr": "falling back: " + MESSAGES["collision"]}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                direct.require_rejection(dict(good, **change), "collision")

    def test_expected_syntax_classes_allow_trailer_only_with_specific_diagnostic(self):
        trailer = "\nERROR: Error computing the main repository mapping: syntax error in MODULE.bazel file for <root>\n"
        messages = {
            "syntax_kwargs": (MESSAGES["syntax_kwargs"], "**kwargs arguments must be a literal dict in MODULE.bazel files."),
            "syntax_star": (MESSAGES["syntax_star"],),
            "syntax_rebinding": (MESSAGES["syntax_rebinding"], "cannot reassign global 'x'", "name 'x' is redeclared", "'x' redeclared at top level"),
        }
        for diagnostic, examples in messages.items():
            for message in examples:
                for suffix in ("", trailer):
                    with self.subTest(diagnostic=diagnostic, message=message, suffix=suffix):
                        direct.require_rejection(record(stderr="ERROR: MODULE.bazel:4:1: " + message + suffix,
                                                        returncode=48), diagnostic)
            with self.subTest(diagnostic=diagnostic), self.assertRaises(ValueError):
                direct.require_rejection(record(stderr=trailer, returncode=48), diagnostic)

    def test_syntax_class_mismatch_parser_errors_and_crashes_never_pass(self):
        classes = ("syntax_kwargs", "syntax_star", "syntax_rebinding")
        trailer = "\nERROR: syntax error in MODULE.bazel file for <root>\n"
        for diagnostic in classes:
            for wrong in (other for other in classes if other != diagnostic):
                with self.subTest(expected=diagnostic, actual=wrong), self.assertRaises(ValueError):
                    direct.require_rejection(record(stderr=MESSAGES[wrong] + trailer, returncode=48), diagnostic)
            for unrelated in ("parse error", "unexpected token", "panicked", "internal error", "FATAL:", "crashed"):
                with self.subTest(diagnostic=diagnostic, unrelated=unrelated), self.assertRaises(ValueError):
                    direct.require_rejection(record(stderr=MESSAGES[diagnostic] + "\n" + unrelated,
                                                    returncode=48), diagnostic)
            for wrong in ("error: name must be a string", "ERROR: unknown repository",
                          "ERROR: could not download registry MODULE.bazel", "ERROR: syntax error"):
                with self.subTest(diagnostic=diagnostic, wrong=wrong), self.assertRaises(ValueError):
                    direct.require_rejection(record(stderr=wrong, returncode=1), diagnostic)

    def test_syntax_errors_are_still_rejected_for_semantic_classes_and_successful_queries(self):
        for diagnostic in ("declaration_signature", "collision", "ordinary_extension_unsupported"):
            with self.subTest(diagnostic=diagnostic), self.assertRaises(ValueError):
                direct.require_rejection(record(stderr=MESSAGES[diagnostic] + "\nsyntax error", returncode=1), diagnostic)
        with self.assertRaises(ValueError):
            direct.require_query(record("@dep//:data\n", stderr="syntax error"))

    def test_one_sided_rejection_never_claims_parity(self):
        item = next(c for c in direct.CASES if c["name"] == "collision.direct-duplicate")
        bad = record(stderr=MESSAGES["collision"], returncode=1)
        for native, official in ((record(), bad), (bad, record()), (bad, record(stderr="download failed", returncode=1))):
            check = {"status": "FAIL", "parity_claim": False}
            with self.assertRaises(ValueError):
                direct.evaluate(item, native, official, check)
            self.assertFalse(check["parity_claim"])
            self.assertNotEqual(check["status"], "PASS")

    def test_laziness_probe_records_eager_reference_as_boundary_not_parity(self):
        item = next(c for c in direct.CASES if c["expected"] == "laziness")
        native = record("@@decldep+//:data\n")
        check = {}
        direct.evaluate(item, native, record(stderr=MESSAGES["declaration_loaded"], returncode=1), check)
        self.assertEqual(check["status"], "UNSUPPORTED_BOUNDARY")
        self.assertFalse(check["parity_claim"])
        check = {}
        direct.evaluate(item, native, record("@dep//:data\n"), check)
        self.assertEqual(check["status"], "PASS")
        self.assertTrue(check["parity_claim"])
        for stderr in ("network unavailable", "unknown repository", "syntax error: declaration fixture was loaded"):
            check = {}
            with self.assertRaises(ValueError):
                direct.evaluate(item, native, record(stderr=stderr, returncode=1), check)
            self.assertEqual(check["status"], "REFERENCE_FAILURE")

    def test_ordinary_extension_definition_is_native_only_and_requires_specific_capability(self):
        item = direct.CASES[-1]
        check = {}
        direct.evaluate(item, record(stderr=RECORDED_ORDINARY_EXTENSION, returncode=1), None, check)
        self.assertFalse(check["parity_claim"])
        self.assertEqual(check["status"], "PASS")
        self.assertIn("module-extension definition remains unsupported", check["details"])
        self.assertIn("supported repository rule", check["details"])
        for result in (record("@@generated//:data\n"), record(stderr="unknown apparent repository generated", returncode=1)):
            with self.assertRaises(ValueError):
                direct.evaluate(item, result, None, {})


class InvocationTests(unittest.TestCase):
    def test_direct_commands_private_caches_explicit_flags_and_no_overrides(self):
        runner = mock.Mock()
        reference = direct.DirectReference(Path("/official"), Path("/workspace"), Path("/scratch"), runner, "r")
        reference.registry = Path("/private/registry space")
        reference.run("q", "query", "--ignore_dev_dependency=true", "@dep//:data")
        argv = runner.run.call_args.args[1]
        for flag in ("--ignore_all_rc_files", "--output_base=/scratch/r-output-base",
                     "--output_user_root=/scratch/r-output-user-root", "--remote_cache=", "--remote_executor=",
                     "--bes_backend=", "--repository_cache=/scratch/repository-cache",
                     "--repo_contents_cache=/scratch/repository-contents", "--ignore_dev_dependency=true"):
            self.assertIn(flag, argv)
        self.assertEqual([arg for arg in argv if arg.startswith("--registry=")], [
            "--registry=file:///private/registry%20space", "--registry=https://bcr.bazel.build"])
        native = direct.native_command(Path("/native"), Path("/scratch/cache"), reference.registry, direct.CASES[0])
        self.assertEqual(native, ["/native", "--ignore_all_rc_files", "--cache-dir=/scratch/cache", "query",
                                  "--lockfile_mode=off", "--registry=file:///private/registry%20space",
                                  "--ignore_dev_dependency=false", "@dep//:data"])
        self.assertFalse(any("override" in arg for arg in argv + native))

    def test_defaults_aliases_and_positive_finite_timeout(self):
        parser = direct.argument_parser()
        args = parser.parse_args([])
        self.assertEqual(args.output.name, "direct-repositories-validation.json")
        self.assertEqual(parser.parse_args(["--report", "/a"]).output, Path("/a"))
        self.assertEqual(parser.parse_args(["--output", "/b"]).output, Path("/b"))
        for value in ("0", "-1", "nan", "inf"):
            with self.assertRaises(argparse.ArgumentTypeError):
                direct.positive_seconds(value)

    def test_both_direct_elf_binaries_and_reference_hash_are_required(self):
        args = direct.argument_parser().parse_args([])
        with mock.patch.object(direct, "validate_executable") as validate, \
                mock.patch.object(direct, "file_evidence", return_value={"sha256": "wrong"}):
            with self.assertRaisesRegex(ValueError, "reference SHA256"):
                direct.validate_binaries(args)
        self.assertEqual(validate.call_args_list, [mock.call(args.native, official=True), mock.call(args.reference, official=True)])
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            path = Path(temporary) / "wrapper"
            path.write_text("#!/bin/sh\nexit 0\n")
            path.chmod(0o755)
            with self.assertRaisesRegex(ValueError, "ELF"):
                direct.validate_executable(path, official=True)

    def test_output_hashes_survive_exception_and_preserve_non_utf8(self):
        report = {"runs": []}
        runner = direct.RecordingRunner(report, 1)

        def failure(*args, **kwargs):
            report["runs"].append(record(stdout="replacement", stdout_base64=base64.b64encode(b"\xff\x00").decode()))
            raise RuntimeError("interrupted runner")

        with mock.patch.object(direct.Runner, "run", side_effect=failure), self.assertRaises(RuntimeError):
            runner.run("failure", ["unused"], Path("/unused"))
        self.assertEqual(report["runs"][0]["stdout_sha256"], hashlib.sha256(b"\xff\x00").hexdigest())
        self.assertEqual(report["runs"][0]["stderr_sha256"], hashlib.sha256(b"").hexdigest())


class MainTests(unittest.TestCase):
    def run_main(self, directory, *, observe=None, fault=None, eager=False):
        """Real main, fixture writer, evaluator, lifecycle, and report; fake subprocess seam only."""
        output = Path(directory) / "report.json"
        observed = []

        def run(runner, name, argv, cwd, timeout=None):
            cwd = Path(cwd)
            if name == "reference.version":
                result = record("bazel 9.3.0\n" if fault != "version" else "bazel 9.1.1\n")
            elif name.endswith(".shutdown"):
                if fault == "shutdown":
                    raise ValueError("injected shutdown failure")
                result = record()
            else:
                case_name, engine = name.rsplit(".", 1)
                if fault == "interrupt":
                    raise KeyboardInterrupt
                if fault == "unexpected":
                    raise RuntimeError("unexpected harness failure")
                item = next(c for c in direct.CASES if c["name"] == case_name)
                registry_arg = next(arg for arg in argv if arg.startswith("--registry=file:"))
                registry = Path(unquote(urlparse(registry_arg.split("=", 1)[1]).path))
                entry = registry / "modules/decldep/1.0"
                descriptor = json.loads((entry / "source.json").read_text())
                source = Path(descriptor["path"])
                observation = {"case": case_name, "engine": engine, "argv": argv, "cwd": cwd,
                               "root": (cwd / "MODULE.bazel").read_text(),
                               "dependency": (entry / "MODULE.bazel").read_text(),
                               "source_module": (source / "MODULE.bazel").read_text(),
                               "source": source, "registry": registry,
                               "defs": (cwd / "defs.bzl").read_text()}
                observed.append(observation)
                if observe:
                    observe(observation)
                if fault == "source-change":
                    (source / "data").write_bytes(b"unexpected mutation")
                if item["expected"] in ("rejection", "native_boundary"):
                    result = record(stderr=MESSAGES[item["diagnostic_class"]], returncode=1)
                    # Exercise captured real-engine wording through main as well
                    # as the isolated classifier tests above.
                    if case_name in ("negative.declaration-label-type", "negative.declaration-rule-type"):
                        index = (0 if case_name.endswith("label-type") else 1) + (2 if engine == "reference" else 0)
                        code, message = RECORDED_DECLARATION_TYPES[index]
                        result = record(stderr=message, returncode=code)
                    elif item["diagnostic_class"] == "collision":
                        code, message = RECORDED_COLLISIONS[4 if engine == "reference" else 0]
                        result = record(stderr=message, returncode=code)
                    elif item["diagnostic_class"] == "ordinary_extension_unsupported":
                        result = record(stderr=RECORDED_ORDINARY_EXTENSION, returncode=1)
                    elif item["diagnostic_class"].startswith("syntax_"):
                        if fault == "wrong-syntax-" + engine:
                            wrong = "syntax_star" if item["diagnostic_class"] == "syntax_kwargs" else "syntax_kwargs"
                            result["stderr"] = MESSAGES[wrong]
                        if engine == "reference":
                            result["returncode"] = 48
                            result["stderr"] += "\nERROR: syntax error in MODULE.bazel file for <root>\n"
                elif eager and item["expected"] == "laziness" and engine == "reference":
                    result = record(stderr=MESSAGES["declaration_loaded"], returncode=1)
                else:
                    result = record("@@decldep+//:data\n" if engine == "native" else "@dep//:data\n")
                if fault == "reference" and engine == "reference":
                    result = record(stderr="BCR unavailable", returncode=1)
            result.update(id=len(runner.report["runs"]), name=name, argv=list(argv), cwd=str(cwd))
            runner.report["runs"].append(result)
            return result

        binary_evidence = {"native": {"sha256": "test-native"}, "reference": {"sha256": direct.REFERENCE_SHA256}}
        with mock.patch.object(direct, "validate_binaries", return_value=binary_evidence), \
                mock.patch.object(direct.Runner, "run", new=run), mock.patch("builtins.print"):
            code = direct.main(["--scratch-parent", directory, "--output", str(output)])
        report = json.loads(output.read_text())
        self.assertTrue(report["scratch_cleaned"])
        self.assertTrue(report["reference_shutdown_attempted"])
        self.assertFalse(Path(report["scratch_directory"]).exists())
        self.assertTrue(report["sources_unchanged"])
        return code, report, observed

    def test_main_wires_actual_root_dependency_flags_and_registry_source_bytes(self):
        # Hand-authored expectations at the CLI seam, not the case builder's output.
        root = 'module(name = "m", version = "1.0")\nbazel_dep(name = "decldep", version = "1.0", repo_name = "dep")\n'
        dependency = 'module(name = "decldep", version = "1.0")\n'
        proxy = 'repo = use_repo_rule("//:missing.bzl", "unknown_rule")\n'
        body = 'repo = use_repo_rule("//:missing.bzl", "unknown_rule")\nrepo(name = "generated", dev_dependency = True, foo = repo)\n'
        matrix = {
            "baseline": (root, dependency, False),
            "ignored.root-ignore": (root + body, dependency, True),
            "ignored.dependency": (root, dependency + body, False),
            "ignored-tag.root-ignore": (root + 'ext=use_extension("//:missing.bzl","ext",dev_dependency=True)\next.tag(value=ext)\n', dependency, True),
            "ignored-tag.dependency": (root, dependency + 'ext=use_extension("//:missing.bzl","ext",dev_dependency=True)\next.tag(value=ext)\n', False),
            "ignored-duplicates.root-ignore": (root + body + 'repo(name = "generated", dev_dependency = True, foo = repo)\n', dependency, True),
            "ignored-duplicates.dependency": (root, dependency + body + 'repo(name = "generated", dev_dependency = True, foo = repo)\n', False),
            "unused.bad-label": (root + 'unused = use_repo_rule("not a label @@@", "rule")\n', dependency, False),
            "negative.before-module": ('repo = use_repo_rule("//:missing.bzl", "unknown_rule")\n' + root, dependency, False),
            "grammar.kwargs-variable": (root + proxy + 'kw={"name":"ignored","dev_dependency":True}\nrepo(**kw)\n', dependency, True),
            "grammar.star-literal": (root + proxy + 'repo(*[])\n', dependency, True),
            "grammar.global-rebinding": (root + proxy + 'x=1\nx=2\n', dependency, True),
            "grammar.global-augmented-rebinding": (root + proxy + 'xs=[]\nxs+=[]\n', dependency, True),
            "grammar.dead-comprehension-kwargs": (root + proxy + 'kw={}\nunused=[repo(**kw) for item in []]\n', dependency, True),
            "grammar.nested-dead-print-kwargs": (root + proxy + 'kw={}\nunused=[str(print(**kw)) for item in []]\n', dependency, True),
            "grammar.literal-computed-key": (root + proxy + 'key="name"\nrepo(**{key:"ignored","dev_dependency":True})\n', dependency, True),
            "grammar.index-mutation": (root + proxy + 'xs=["before"]\nxs[0]="after"\nrepo(name="ignored",dev_dependency=True,foo=xs)\n', dependency, True),
        }
        seen = set()

        def inspect(observation):
            name = observation["case"]
            if name in matrix:
                expected_root, expected_dependency, ignore = matrix[name]
                self.assertEqual(observation["root"], expected_root, name)
                self.assertEqual(observation["dependency"], expected_dependency, name)
                self.assertEqual(observation["source_module"], expected_dependency, name)
                self.assertIn("--ignore_dev_dependency=" + str(ignore).lower(), observation["argv"])
                seen.add((name, observation["engine"]))
            self.assertNotIn("local_path_override", observation["root"])
            self.assertEqual((observation["source"] / "BUILD.bazel").read_bytes(), b'exports_files(["data"])\n')
            self.assertEqual((observation["source"] / "data").read_bytes(), b"direct repository declaration fixture\x00\xff\n")
            self.assertTrue(observation["source"].is_relative_to(observation["registry"].parent))
            if name == "active-unused.load-probe":
                self.assertIn('fail("declaration fixture was loaded")', observation["defs"])
                self.assertIn('repo(name = "generated")', observation["root"])
            elif name == "native.ordinary-extension-definition-boundary":
                self.assertNotIn("fail(", observation["defs"])
                self.assertIn("repository_rule(implementation = _repo_impl)", observation["defs"])
                self.assertIn("module_extension(implementation = _ext_impl)", observation["defs"])
                self.assertEqual(observation["defs"], direct.REAL_DEFS)
                self.assertEqual(observation["argv"][-1], "@generated//:data")
                self.assertEqual(observation["engine"], "native")
            else:
                self.assertEqual(observation["argv"][-1], "@dep//:data")

        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            code, report, observations = self.run_main(temporary, observe=inspect)
        self.assertEqual(code, 0, report["errors"])
        self.assertEqual(report["status"], "PASS")
        self.assertEqual(seen, {(name, engine) for name in matrix for engine in ("native", "reference")})
        self.assertEqual(len(observations), 95)
        self.assertEqual(len({str(o["registry"]) for o in observations}), 48)
        self.assertEqual(len({str(o["cwd"]) for o in observations}), 1)
        self.assertEqual(len({arg for o in observations for arg in o["argv"] if arg.startswith("--cache-dir=")}), 48)
        for check in report["checks"]:
            self.assertTrue(check["inputs_unchanged"])
            self.assertEqual(check["files_before"], check["files_after"])
        self.assertEqual(report["completed_differential_cases"], 46)
        self.assertFalse(any("Native generated repositories cannot execute" in text
                             for text in report["limitations"]))
        self.assertTrue(any("module-extension definition remains unsupported" in text
                            for text in report["limitations"]))
        self.assertEqual({c["name"]: (c["expected"], c["input"]["diagnostic_class"])
                          for c in report["checks"] if c["name"].startswith("grammar.")}, {
            "grammar.kwargs-variable": ("rejection", "syntax_kwargs"),
            "grammar.star-literal": ("rejection", "syntax_star"),
            "grammar.global-rebinding": ("rejection", "syntax_rebinding"),
            "grammar.global-augmented-rebinding": ("rejection", "syntax_rebinding"),
            "grammar.dead-comprehension-kwargs": ("rejection", "syntax_kwargs"),
            "grammar.nested-dead-print-kwargs": ("rejection", "syntax_kwargs"),
            "grammar.literal-computed-key": ("query", None),
            "grammar.index-mutation": ("query", None),
        })
        self.assertFalse(report["test_commands_executed"] or report["build_commands_executed"])
        self.assertEqual(report["case_inputs_sha256"], hashlib.sha256(json.dumps(report["planned_checks"], sort_keys=True).encode()).hexdigest())

    def test_eager_reference_is_visible_nonparity_boundary_in_main_report(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            code, report, _ = self.run_main(temporary, eager=True)
        self.assertEqual(code, 0)
        check = next(c for c in report["checks"] if c["name"] == "active-unused.load-probe")
        self.assertEqual(check["status"], "UNSUPPORTED_BOUNDARY")
        self.assertFalse(check["parity_claim"])
        self.assertEqual(report["completed_differential_cases"], 46)

    def test_wrong_syntax_class_from_either_engine_fails_main_report(self):
        for engine in ("native", "reference"):
            with self.subTest(engine=engine), tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
                code, report, _ = self.run_main(temporary, fault="wrong-syntax-" + engine)
            self.assertEqual(code, 1)
            self.assertEqual(report["status"], "FAIL")
            rejected = [c for c in report["checks"] if c["name"].startswith("grammar.") and c["expected"] == "rejection"]
            self.assertEqual(len(rejected), 6)
            self.assertTrue(all(c["status"] == ("REFERENCE_FAILURE" if engine == "reference" else "FAIL") for c in rejected))
            self.assertTrue(all(c["parity_claim"] is False for c in rejected))

    def test_fail_closed_predicate_rejects_malformed_missing_wrong_and_forged_checks(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            _, report, _ = self.run_main(temporary)
        self.assertTrue(direct.report_passes(report))
        for malformed in ({}, None, [], {"checks": None}, {"checks": [{}]}, {"checks": "PASS"}):
            with self.subTest(malformed=malformed):
                self.assertFalse(direct.report_passes(malformed))
        for mutation in ("missing", "duplicate", "order", "kind", "status", "input", "evidence", "run-id",
                         "wrong-run", "wrong-result", "scratch", "shutdown", "sources", "error", "pin", "parity"):
            changed = copy.deepcopy(report)
            if mutation == "missing":
                changed["checks"].pop()
            elif mutation == "duplicate":
                changed["checks"][-1] = changed["checks"][0]
            elif mutation == "order":
                changed["checks"].reverse()
            elif mutation == "kind":
                changed["checks"][0]["kind"] = "native_capability_boundary"
            elif mutation == "status":
                changed["checks"][0]["status"] = "UNSUPPORTED_BOUNDARY"
            elif mutation == "input":
                changed["checks"][0]["input"]["root_module"] = ""
            elif mutation == "evidence":
                changed["checks"][0]["files_after"] = {}
            elif mutation == "run-id":
                changed["checks"][0]["run_ids"].pop("reference")
            elif mutation == "wrong-run":
                changed["checks"][0]["run_ids"]["native"] = -1
            elif mutation == "wrong-result":
                changed["runs"][changed["checks"][0]["run_ids"]["native"]]["stdout"] = "//:data\n"
            elif mutation in ("scratch", "shutdown", "sources"):
                changed[{"scratch": "scratch_cleaned", "shutdown": "reference_shutdown_attempted", "sources": "sources_unchanged"}[mutation]] = False
            elif mutation == "error":
                changed["errors"].append("cleanup failed")
            elif mutation == "pin":
                changed["executables"]["reference"]["sha256"] = "wrong"
            elif mutation == "parity":
                changed["checks"][-1]["parity_claim"] = True
            with self.subTest(mutation=mutation):
                self.assertFalse(direct.report_passes(changed))

    def test_reference_failure_interrupt_exception_mutation_and_shutdown_fail_closed_and_cleanup(self):
        for fault in ("reference", "interrupt", "unexpected", "source-change", "shutdown", "version"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
                code, report, _ = self.run_main(temporary, fault=fault)
                self.assertEqual(code, 130 if fault == "interrupt" else 1)
                self.assertEqual(report["status"], "FAIL")
                self.assertTrue(report["errors"] or any(c["status"] != "PASS" for c in report["checks"]))
                if fault == "reference":
                    self.assertTrue(any(c["status"] == "REFERENCE_FAILURE" for c in report["checks"]))
                if fault == "source-change":
                    self.assertTrue(all(c["inputs_unchanged"] is False for c in report["checks"]))

    def test_missing_executable_writes_failure_without_any_process(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            output = Path(temporary) / "report.json"
            with mock.patch.object(direct.Runner, "run", side_effect=AssertionError("no subprocess")), mock.patch("builtins.print"):
                code = direct.main(["--native", temporary + "/missing", "--report", str(output)])
            report = json.loads(output.read_text())
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "FAIL")
        self.assertEqual(report["runs"], [])
        self.assertTrue(report["errors"])

    def test_report_cannot_overwrite_binary_sources_or_fixtures(self):
        for destination in (direct.ROOT / "target/release/bazel-rs", Path(direct.__file__),
                            direct.ROOT / "tests/fixtures/bad.json"):
            with self.subTest(destination=destination), mock.patch.object(direct, "validate_binaries") as guard:
                with self.assertRaisesRegex(ValueError, "must not overwrite"):
                    direct.main(["--output", str(destination)])
                guard.assert_not_called()


if __name__ == "__main__":
    unittest.main()
