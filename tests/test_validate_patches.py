"""Python-only harness verification. Synthetic subprocesses are NOT Bazel evidence."""

import argparse
import base64
import copy
import hashlib
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest import mock
from urllib.parse import unquote, urlparse

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import validate_patches as patches


def record(stdout="", stderr="", returncode=0, **extra):
    return dict(id=0, name="synthetic", stdout=stdout, stderr=stderr,
                returncode=returncode, timed_out=False, **extra)


# Synthetic contract examples except the explicitly captured reference line below.
MESSAGES = {
    "missing_package": "ERROR: no such package 'not-a-package': BUILD file not found",
    "missing_file": "error: patch file missing.patch: No such file or directory",
    "unknown_label": "error: unknown apparent repository not_visible for patch label",
    "structure": "error: patch local-0.patch: malformed hunk count",
    "section_boundary": "error: adjacent plain patch sections require an explicit separator",
    "path": "error: patch local-0.patch: unsafe traversal path ../escape",
    "unsupported": "error: patch transform is unsupported for this source",
    "context": "error: patch local-0.patch: hunk context does not match",
    "integrity": "error: patch z-first.patch: malformed integrity digest base64",
}

# Verbatim from the first artifacts/patch-validation.json; no runtime dependency.
RECORDED_UNKNOWN_LABEL = ("Error in single_version_override: invalid label in 'patches': "
                          "only patches in the main repository can be applied, not from '@not_visible'")


def assert_graph_descriptors(test, registry, missing_dependency=False):
    entries = [registry / "modules/opencode_patchcheck_dep/1.0"] + [
        registry / "modules/opencode_patchcheck_leaf" / version for version in ("1.0", "2.0", "3.0")]
    for index, entry in enumerate(entries):
        path = entry / "source.json"
        if index == 0 and missing_dependency:
            test.assertFalse(path.exists())
            continue
        descriptor = json.loads(path.read_text())
        archive = registry.parent / "archive-must-not-be-fetched.tar"
        test.assertEqual(descriptor, {"url": archive.as_uri(), "integrity": "sha256-" + base64.b64encode(
            hashlib.sha256(b"graph source archive deliberately unavailable\n").digest()).decode()})
        test.assertFalse(archive.exists())


def item(name):
    return next(value for value in patches.CASES if value["name"] == name)


def uri_path(uri):
    return Path(unquote(urlparse(uri).path))


def assert_sections(test, text, paths, created=(), deleted=()):
    """Check literal section boundaries and old/new headers, never apply patches."""
    lines = text.splitlines()
    test.assertEqual([line for line in lines if line.startswith("diff --git ")],
                     ["diff --git a/" + path + " b/" + path for path in paths])
    test.assertEqual([line for line in lines if line.startswith("--- ")],
                     ["--- " + ("/dev/null" if path in created else "a/" + path) for path in paths])
    test.assertEqual([line for line in lines if line.startswith("+++ ")],
                     ["+++ " + ("/dev/null" if path in deleted else "b/" + path) for path in paths])
    for index, line in enumerate(lines):
        if line.startswith("diff --git "):
            test.assertTrue(lines[index + 1].startswith("--- "))
            test.assertTrue(lines[index + 2].startswith("+++ "))


class FixtureTests(unittest.TestCase):
    def test_inventory_and_native_only_boundaries(self):
        self.assertEqual(len(patches.CASES), 27)
        self.assertEqual(len(set(name for name, _ in patches.PLAN)), 27)
        self.assertEqual(sum(kind == "differential" for _, kind in patches.PLAN), 18)
        # N26: Bazel skips these sections during discovery; native agrees.
        for name in ("graph.adjacent-plain-sections-skipped", "graph.git-binary-skipped",
                     "graph.rename-skipped", "graph.mode-change-skipped"):
            self.assertEqual(item(name)["kind"], "differential")
            self.assertEqual(item(name)["expected"], "success")
            self.assertIsNone(item(name)["diagnostic"])
        for value in patches.CASES:
            self.assertEqual(value["kind"] == "native_only", value["name"].startswith("native."))
        self.assertEqual(item("graph.ordered-module-patches")["leaf"], "3.0")

    def test_patch_helper_separates_modification_creation_deletion_and_empty_diff(self):
        for path, before, after in (("data", "old\n", "new\n"), ("created", "", "créé\n"),
                                    ("obsolete", "remove me\n", "")):
            with self.subTest(path=path):
                assert_sections(self, patches.patch_text(path, before, after), [path],
                                created=[path] if not before else [], deleted=[path] if not after else [])
        self.assertEqual(patches.patch_text("data", "same\n", "same\n"), "")

    def test_adjacent_plain_boundary_has_two_valid_sections_and_no_git_separator(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            root = Path(temporary)
            value = item("graph.adjacent-plain-sections-skipped")
            fixture = patches.materialize(root / "case", root / "workspace", value)
            text = fixture["patches"][0].read_text()
            self.assertEqual(text, "--- a/data\n+++ b/data\n@@ -1 +1 @@\n-old\n+new\n"
                             "--- a/nested/MODULE.bazel\n+++ b/nested/MODULE.bazel\n"
                             "@@ -1 +1 @@\n-nested old\n+nested new\n")
            self.assertNotIn("diff --git", text)
            self.assertEqual(value["kind"], "differential")
            self.assertIsNone(value["diagnostic"])

    def test_tar_is_deterministic_regular_ustar_and_integrity_pinned(self):
        first = patches.archive_bytes()
        self.assertEqual(first, patches.archive_bytes())
        self.assertEqual(first[257:263], b"ustar\x00")
        with tarfile.open(fileobj=io.BytesIO(first)) as archive:
            self.assertEqual(archive.getnames(), ["pkg/BUILD.bazel", "pkg/MODULE.bazel", "pkg/data", "pkg/obsolete"])
            for member in archive:
                self.assertTrue(member.isfile())
                self.assertEqual((member.mtime, member.mode, member.uid, member.gid), (0, 0o644, 0, 0))
            self.assertEqual(archive.extractfile("pkg/data").read(), "header\narchive café\nfooter\n".encode())
        self.assertEqual(patches.sri(first), "sha256-" + base64.b64encode(hashlib.sha256(first).digest()).decode())

    def test_descriptor_order_and_original_module_are_preserved(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            root = Path(temporary)
            fixture = patches.materialize(root / "case", root / "workspace", item("materialize.transform-order"))
            entry = fixture["entry"]
            descriptor = json.loads((entry / "source.json").read_text())
            self.assertEqual(list(descriptor["patches"]), ["z-first.patch", "a-second.patch"])
            self.assertEqual(descriptor["patch_strip"], 1)
            self.assertEqual(descriptor["integrity"], patches.sri(uri_path(descriptor["url"]).read_bytes()))
            self.assertEqual((entry / "MODULE.bazel").read_text(), patches.MODULE)
            self.assertNotEqual((entry / "overlay/MODULE.bazel").read_text(), patches.MODULE)
            for name, integrity in descriptor["patches"].items():
                self.assertEqual(integrity, patches.sri((entry / "patches" / name).read_bytes()))
            assert_sections(self, (entry / "patches/z-first.patch").read_text(), ["data", "MODULE.bazel"])
            assert_sections(self, (entry / "patches/a-second.patch").read_text(), ["data"])
            local = (root / "workspace/local-0.patch").read_text()
            assert_sections(self, local, ["MODULE.bazel", "data", "created", "obsolete"],
                            created=["created"], deleted=["obsolete"])
            self.assertIn("-# registry authority café\n+# local final café", local)
            self.assertIn("@@ -8,3 +8,3 @@\n header\n-remote two café\n+local final café\n footer", local)
            self.assertIn("--- /dev/null\n+++ b/created", local)
            self.assertIn("--- a/obsolete\n+++ /dev/null", local)
            self.assertTrue(all(path.read_bytes() == contents for path, contents in fixture["files"].items()))
            self.assertFalse((fixture["registry"] / "modules/bazel_tools").exists())
            self.assertFalse(fixture["marker"].exists())

    def test_graph_fixtures_have_descriptors_but_no_archives_and_real_patch_labels(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            root = Path(temporary)
            registries = []
            for value in (value for value in patches.CASES if value["mode"] == "graph"):
                fixture = patches.materialize(root / value["name"], root / "workspace", value)
                registries.append(fixture["registry"].as_uri())
                missing = value["name"] == "native.nonmodule-no-source"
                assert_graph_descriptors(self, fixture["registry"], missing)
                self.assertEqual(fixture["descriptor"] is None, missing)
                original = patches.module_dep(value["module_start"]) if value.get("module_start") else patches.MODULE
                self.assertEqual((fixture["entry"] / "MODULE.bazel").read_text(), original)
                for path in fixture["patches"]:
                    self.assertTrue(path.is_file())
                module = (root / "workspace/MODULE.bazel").read_text()
                self.assertIn("single_version_override", module)
                self.assertNotIn("local_path_override", module)
                if value["name"] == "graph.ordered-module-patches":
                    self.assertIn('["//:local-0.patch", "//:local-1.patch"]', module)
                    self.assertIn('version = "2.0"', fixture["patches"][0].read_text())
                    self.assertIn('version = "3.0"', fixture["patches"][1].read_text())
                if value.get("self_alias"):
                    self.assertIn('"@opencode_patchcheck_root//:local-0.patch"', module)
                if value.get("mixed"):
                    text = fixture["patches"][0].read_text()
                    assert_sections(self, text, ["MODULE.bazel", "data", "nested/MODULE.bazel"])
                    for name in ("MODULE.bazel", "data", "nested/MODULE.bazel"):
                        self.assertIn("--- a/" + name, text)
            self.assertEqual(len(registries), len(set(registries)))

    def test_lexical_strip_headers_and_explicit_expected_versions(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            root = Path(temporary)
            for name, old, new, strip, expected_version in (
                    ("graph.dot-prefix-is-not-root", "./a/MODULE.bazel", "./b/MODULE.bazel", 1, "1.0"),
                    ("graph.empty-component-strip", "a//MODULE.bazel", "b//MODULE.bazel", 2, "2.0")):
                with self.subTest(name=name):
                    value = item(name)
                    fixture = patches.materialize(root / name, root / "workspace", value)
                    text = fixture["patches"][0].read_text()
                    self.assertTrue(text.startswith("diff --git " + old + " " + new + "\n"
                                                    "--- " + old + "\n+++ " + new + "\n"))
                    self.assertIn('-bazel_dep(name = "opencode_patchcheck_leaf", version = "1.0")\n', text)
                    self.assertIn('+bazel_dep(name = "opencode_patchcheck_leaf", version = "2.0")\n', text)
                    self.assertIn("patch_strip = " + str(strip), (root / "workspace/MODULE.bazel").read_text())
                    leaf = patches.graph_expected(value)["dependencies"][0]["dependencies"][0]
                    self.assertEqual(leaf["version"], expected_version)
                    self.assertEqual(leaf["key"], "opencode_patchcheck_leaf@" + expected_version)

    def test_malformed_remote_integrity_is_descriptor_rejection_not_download_mismatch(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            root = Path(temporary)
            fixture = patches.materialize(root / "case", root / "workspace",
                                          item("native.remote-integrity-descriptor-rejected"))
            self.assertEqual(fixture["descriptor"]["patches"]["z-first.patch"], "sha256-not-base64!")

    def test_negative_fixture_is_missing_package_not_missing_patch(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            root = Path(temporary)
            patches.materialize(root / "case", root / "workspace", item("negative.missing-package"))
            self.assertTrue((root / "workspace/not-a-package/missing.patch").is_file())
            self.assertFalse((root / "workspace/not-a-package/BUILD.bazel").exists())

    def test_local_path_boundary_inputs_are_private_and_snapshot_detects_mutation(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            root = Path(temporary)
            fixture = patches.materialize(root / "case", root / "workspace", item("native.local-path-transform"))
            source = Path(fixture["descriptor"]["path"])
            self.assertTrue(source.is_relative_to(root))
            before = patches.snapshot(fixture["files"])
            evidence = before[str(source / "data")]
            self.assertEqual(base64.b64decode(evidence["bytes_base64"]), patches.data("remote two"))
            self.assertTrue(patches.evidence_matches(evidence))
            (source / "data").write_bytes(b"mutated")
            self.assertNotEqual(before, patches.snapshot(fixture["files"]))


class ValidationTests(unittest.TestCase):
    def test_section_boundary_diagnostic_requires_context_and_rejects_other_failures(self):
        for message in (MESSAGES["section_boundary"], "unsupported patch section boundary",
                        "ambiguous adjacent patch sections", "patch sections need an explicit separator"):
            patches.require_rejection(record(stderr=message, returncode=1), "section_boundary")
        for message in ("unsupported patch format", "separator required", "malformed hunk count",
                        "failed to download patch: missing separator", "FATAL: ambiguous patch"):
            with self.subTest(message=message), self.assertRaises(ValueError):
                patches.require_rejection(record(stderr=message, returncode=1), "section_boundary")

    def test_captured_reference_unknown_label_is_narrow_and_not_other_failures(self):
        patches.require_rejection(record(stderr=RECORDED_UNKNOWN_LABEL, returncode=37), "unknown_label")
        for diagnostic in MESSAGES:
            if diagnostic != "unknown_label":
                with self.subTest(diagnostic=diagnostic), self.assertRaises(ValueError):
                    patches.require_rejection(record(stderr=RECORDED_UNKNOWN_LABEL, returncode=37), diagnostic)
        for stderr in ("invalid label in 'patches'", "only patches in the main repository can be applied",
                       RECORDED_UNKNOWN_LABEL.replace("'patches'", "'other'"),
                       "failed to download MODULE.bazel\n" + RECORDED_UNKNOWN_LABEL,
                       "FATAL: " + RECORDED_UNKNOWN_LABEL):
            with self.subTest(stderr=stderr), self.assertRaises(ValueError):
                patches.require_rejection(record(stderr=stderr, returncode=37), "unknown_label")

    def test_semantic_rejections_and_fail_closed_unrelated_errors(self):
        for diagnostic, message in MESSAGES.items():
            with self.subTest(diagnostic=diagnostic):
                good = record(stderr=message, returncode=48)
                patches.require_rejection(good, diagnostic)
                for change in ({"returncode": 0}, {"returncode": True}, {"returncode": 101},
                               {"returncode": None}, {"returncode": -9}, {"returncode": 139},
                               {"timed_out": True}, {"spawn_error": "missing"}, {"interrupted": True},
                               {"stdout": "success"}, {"stderr": "unrelated registry missing"},
                               {"stderr": "failed to download MODULE.bazel\n" + message},
                               {"stderr": "SSLHandshakeException\n" + message},
                               {"stderr": "panicked: " + message}, {"stderr": "syntax error: " + message},
                               {"stderr": "falling back: " + message}):
                    with self.subTest(change=change), self.assertRaises(ValueError):
                        patches.require_rejection(dict(good, **change), diagnostic)

    def test_query_identity_and_graph_exactness(self):
        patches.query_result(record("@patched//:sources\n@patched//:copy\n"))
        patches.query_result(record("@@opencode_patchcheck_dep+//:copy\n@@opencode_patchcheck_dep+//:sources\n"))
        patches.query_result(record("@patched//:copy\n@patched//:sources\n"), native=True)
        for text in ("", "@patched//:copy\n", "//:sources\n//:copy\n", "@patched//:copy\n@patched//:copy\n"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                patches.query_result(record(text))
        for text in ("@@opencode_patchcheck_dep+//:copy\n@@opencode_patchcheck_dep+//:sources\n",
                     "@patched//:sources\n@patched//:copy\n"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                patches.query_result(record(text), native=True)
        graph = patches.graph_expected(item("graph.dependency-before-mvs"))
        leaf = graph["dependencies"][0]["dependencies"][0]
        self.assertEqual(leaf["key"], "opencode_patchcheck_leaf@2.0")
        other = copy.deepcopy(graph)
        other["dependencies"][0]["dependencies"][0]["version"] = "1.0"
        with self.assertRaises(ValueError):
            patches.compare_graphs(graph, other, graph)

    def test_no_publication_rejects_hidden_staging_files(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            cache = Path(temporary)
            patches.no_publication(cache)
            path = cache / "repositories/trees/.staging"
            path.mkdir(parents=True)
            with self.assertRaises(ValueError):
                patches.no_publication(cache)

    def test_manifest_and_native_outputs_refuse_identity_or_byte_loss(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            root = Path(temporary)
            path = root / "data"
            path.write_bytes(b"exact\n")
            self.assertEqual(set(patches.manifest([path], {"data": b"exact\n"}, root)), {"data"})
            for paths, expected in (([], {"data": b"exact\n"}), ([path, path], {"data": b"exact\n"}),
                                    ([path], {"data": b"wrong"}), ([path], {"wrong": b"exact\n"})):
                with self.assertRaises(ValueError):
                    patches.manifest(paths, expected, root)
            target = "@patched//:sources"
            good = patches.canonical(target) + "\t" + str(path) + "\n"
            self.assertEqual(patches.native_paths(record(good), target, root), [path])
            for text in (good + good, "@patched//:sources\t" + str(path), patches.canonical(target) + "\t/etc/passwd"):
                with self.assertRaises(ValueError):
                    patches.native_paths(record(text), target, root)

    def test_commands_direct_graph_position_shared_caches_no_wrappers(self):
        runner = mock.Mock()
        ref = patches.PatchReference(Path("/official"), Path("/workspace"), Path("/scratch"), runner, "patches")
        ref.registry = Path("/case space/registry")
        ref.run("graph", "mod")
        argv = runner.run.call_args.args[1]
        self.assertEqual(argv[argv.index("mod"):argv.index("mod") + 2], ["mod", "graph"])
        for flag in ("--ignore_all_rc_files", "--output_base=/scratch/patches-output-base",
                     "--output_user_root=/scratch/patches-output-user-root", "--remote_cache=",
                     "--remote_executor=", "--bes_backend=", "--repository_cache=/scratch/repository-cache",
                     "--repo_contents_cache=/scratch/repository-contents", *patches.PROFILE):
            self.assertIn(flag, argv)
        self.assertEqual([arg for arg in argv if arg.startswith("--registry=")], [
            "--registry=file:///case%20space/registry", "--registry=https://bcr.bazel.build"])
        native = patches.native_command(Path("/native"), Path("/scratch/native"), ref.registry, "mod")
        self.assertEqual(native[:5], ["/native", "--ignore_all_rc_files", "--cache-dir=/scratch/native", "mod", "graph"])
        self.assertIn("--lockfile_mode=off", native)
        self.assertFalse(any("override" in arg for arg in argv + native))

    def test_arguments_direct_elf_and_pinned_hash(self):
        args = patches.argument_parser().parse_args([])
        self.assertEqual(args.output.name, "patch-validation.json")
        for value in ("0", "-1", "nan", "inf"):
            with self.assertRaises(argparse.ArgumentTypeError):
                patches.positive_seconds(value)
        with mock.patch.object(patches, "validate_executable") as validate, \
                mock.patch.object(patches, "file_evidence", return_value={"sha256": "wrong"}):
            with self.assertRaisesRegex(ValueError, "reference SHA256"):
                patches.validate_binaries(args)
        self.assertEqual(validate.call_args_list, [mock.call(args.native, official=True), mock.call(args.reference, official=True)])
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            path = Path(temporary) / "wrapper"
            path.write_text("#!/bin/sh\nexit 0\n")
            path.chmod(0o755)
            with self.assertRaisesRegex(ValueError, "ELF"):
                patches.validate_executable(path, official=True)

    def test_recording_hashes_exact_non_utf8_even_on_interruption(self):
        report = {"runs": []}
        runner = patches.RecordingRunner(report, 1)

        def fail(*args):
            report["runs"].append(record(stdout="replacement", stdout_base64=base64.b64encode(b"\xff\x00").decode()))
            raise KeyboardInterrupt

        with mock.patch.object(patches.Runner, "run", side_effect=fail), self.assertRaises(KeyboardInterrupt):
            runner.run("r", ["unused"], Path("/unused"))
        self.assertEqual(report["runs"][0]["stdout_sha256"], hashlib.sha256(b"\xff\x00").hexdigest())


class MainTests(unittest.TestCase):
    def run_main(self, directory, fault=None, observe=None):
        """Real fixture writer/main/evaluator, synthetic Runner only; never spawn Bazel."""
        output = Path(directory) / "report.json"
        seen = []

        def run(runner, name, argv, cwd, timeout=None):
            cwd = Path(cwd)
            if name == "reference.version":
                result = record("bazel 9.3.0\n" if fault != "version" else "bazel 9.2.0\n")
            elif name == "patches.shutdown":
                if fault == "shutdown":
                    raise ValueError("injected shutdown failure")
                result = record()
            else:
                if fault == "interrupt":
                    raise KeyboardInterrupt
                if fault == "unexpected":
                    raise RuntimeError("injected unexpected exception")
                case_name, rest = next((value["name"], name[len(value["name"]) + 1:])
                                       for value in patches.CASES if name.startswith(value["name"] + "."))
                value = item(case_name)
                engine, phase = rest.split(".", 1)
                registry = uri_path(next(arg.split("=", 1)[1] for arg in argv if arg.startswith("--registry=file:")))
                entry = registry / "modules/opencode_patchcheck_dep/1.0"
                descriptor = json.loads((entry / "source.json").read_text()) if (entry / "source.json").exists() else None
                observation = dict(name=case_name, engine=engine, phase=phase, argv=argv, cwd=cwd,
                                   registry=registry, entry=entry, descriptor=descriptor,
                                   root=(cwd / "MODULE.bazel").read_text())
                seen.append(observation)
                if observe:
                    observe(observation)
                if fault == "mutation":
                    (entry / "MODULE.bazel").write_bytes(b"mutated")
                if fault == "marker":
                    (registry.parent / "COMMAND_MUST_NOT_RUN").write_bytes(b"bad")
                if value["expected"] == "reject":
                    result = record(stderr=MESSAGES[value["diagnostic"]], returncode=48 if engine == "reference" else 1)
                    if value["diagnostic"] == "unknown_label" and engine == "reference":
                        result = record(stderr=RECORDED_UNKNOWN_LABEL, returncode=37)
                    if fault == "publication":
                        cache = Path(next(a.split("=", 1)[1] for a in argv if a.startswith("--cache-dir="))) if engine == "native" else None
                        if cache:
                            (cache / "repositories/trees/.leaked").mkdir(parents=True, exist_ok=True)
                elif value["mode"] == "graph":
                    result = record(json.dumps(patches.graph_expected(value)))
                elif phase == "query":
                    prefix = "@patched"  # both engines print Bazel's display form (N39)
                    result = record(prefix + "//:copy\n" + prefix + "//:sources\n")
                else:
                    # Explicit expected fixture bytes, not a patch applier or fabricated CLI evidence.
                    final = "edited final" if phase.startswith("edited.") else "local final"
                    source_bytes = {
                        "MODULE.bazel": 'module(name = "opencode_patchcheck_dep", version = "1.0")\n# local final café\n'.encode(),
                        "data": ("header\n" + final + " café\nfooter\n").encode(), "created": "créé\n".encode()}
                    source_target = "sources" in phase
                    if engine == "native":
                        cache = Path(next(a.split("=", 1)[1] for a in argv if a.startswith("--cache-dir=")))
                        tree = cache / "repositories/trees" / ("edited-key" if final == "edited final" else "original-key")
                        for filename, contents in source_bytes.items():
                            path = tree / filename
                            path.parent.mkdir(parents=True, exist_ok=True)
                            path.write_bytes(contents)
                        paths = [tree / name for name in source_bytes] if source_target else [
                            cache / "bazel-out/k8-fastbuild/bin/external/opencode_patchcheck_dep+/copy.txt"]
                    else:
                        output_base = Path(next(a.split("=", 1)[1] for a in argv if a.startswith("--output_base=")))
                        execroot = output_base / "execroot/_main"
                        tree = execroot / "external/opencode_patchcheck_dep+"
                        paths = [tree / name for name in source_bytes] if source_target else [
                            execroot / "bazel-out/k8-fastbuild/bin/external/opencode_patchcheck_dep+/copy.txt"]
                    for path in paths:
                        path.parent.mkdir(parents=True, exist_ok=True)
                        path.write_bytes(source_bytes[path.name] if source_target else source_bytes["data"])
                        if fault == "bytes":
                            path.write_bytes(b"incorrect\n")
                    if engine == "native":
                        label = "@@opencode_patchcheck_dep+//:" + ("sources" if source_target else "copy")
                        result = record("".join(label + "\t" + str(path) + "\n" for path in paths))
                    else:
                        result = record("".join(path.relative_to(execroot).as_posix() + "\n" for path in paths)
                                        if phase.endswith(".files") else "")
                if fault == "reference" and engine == "reference":
                    result = record(stderr="failed to download genuine built-in: network unavailable", returncode=1)
            result.update(id=len(runner.report["runs"]), name=name, argv=list(argv), cwd=str(cwd))
            runner.report["runs"].append(result)
            return result

        evidence = {"native": {"sha256": "test-native"}, "reference": {"sha256": patches.REFERENCE_SHA256}}
        with mock.patch.object(patches, "validate_binaries", return_value=evidence), \
                mock.patch.object(patches.Runner, "run", new=run), mock.patch("builtins.print"):
            code = patches.main(["--scratch-parent", directory, "--output", str(output)])
        report = json.loads(output.read_text())
        self.assertTrue(report["scratch_cleaned"])
        self.assertTrue(report["reference_shutdown_attempted"])
        self.assertTrue(report["sources_unchanged"])
        self.assertFalse(Path(report["scratch_directory"]).exists())
        return code, report, seen

    def test_actual_main_wires_descriptor_local_patches_and_build_bytes(self):
        lexical_seen = set()

        def observe(o):
            self.assertIn('module(name = "opencode_patchcheck_root", version = "1.0")', o["root"])
            self.assertIn('repo_name = "patched"', o["root"])
            self.assertIn('patch_strip = ' + ("2" if o["name"] == "graph.empty-component-strip" else "1"), o["root"])
            if o["descriptor"] and "patches" in o["descriptor"]:
                self.assertEqual(list(o["descriptor"]["patches"]), ["z-first.patch", "a-second.patch"])
                assert_sections(self, (o["entry"] / "patches/z-first.patch").read_text(), ["data", "MODULE.bazel"])
                assert_sections(self, (o["entry"] / "patches/a-second.patch").read_text(), ["data"])
                archive_path = uri_path(o["descriptor"]["url"])
                self.assertTrue(archive_path.is_relative_to(o["registry"].parent))
                self.assertEqual(o["descriptor"]["integrity"], patches.sri(archive_path.read_bytes()))
                self.assertEqual((o["entry"] / "MODULE.bazel").read_text(), patches.MODULE)
                local = (o["cwd"] / "local-0.patch").read_text()
                assert_sections(self, local, ["MODULE.bazel", "data", "created", "obsolete"],
                                created=["created"], deleted=["obsolete"])
                self.assertIn("-# registry authority café\n+# local final café", local)
                self.assertIn("+edited final café" if o["phase"].startswith("edited.") else "+local final café", local)
            elif item(o["name"])["mode"] == "graph":
                assert_graph_descriptors(self, o["registry"], o["name"] == "native.nonmodule-no-source")
                self.assertEqual(o["descriptor"] is None, o["name"] == "native.nonmodule-no-source")
            if o["name"] in ("graph.dot-prefix-is-not-root", "graph.empty-component-strip"):
                prefix = "./a/" if o["name"] == "graph.dot-prefix-is-not-root" else "a//"
                new_prefix = "./b/" if o["name"] == "graph.dot-prefix-is-not-root" else "b//"
                text = (o["cwd"] / "local-0.patch").read_text()
                self.assertTrue(text.startswith("diff --git " + prefix + "MODULE.bazel " + new_prefix + "MODULE.bazel\n"
                                                "--- " + prefix + "MODULE.bazel\n+++ " + new_prefix + "MODULE.bazel\n"))
                self.assertIn('-bazel_dep(name = "opencode_patchcheck_leaf", version = "1.0")', text)
                self.assertIn('+bazel_dep(name = "opencode_patchcheck_leaf", version = "2.0")', text)
                lexical_seen.add((o["name"], o["engine"]))
            if o["name"] == "graph.ordered-module-patches":
                self.assertIn('["//:local-0.patch", "//:local-1.patch"]', o["root"])
            if o["name"] == "graph.canonical-external-skipped":
                self.assertIn('"@@opencode_patchcheck_absent+//:missing.patch"', o["root"])
            if o["name"] == "graph.mixed-root-source-nested":
                assert_sections(self, (o["cwd"] / "local-0.patch").read_text(),
                                ["MODULE.bazel", "data", "nested/MODULE.bazel"])
            if o["name"] == "graph.adjacent-plain-sections-skipped":
                text = (o["cwd"] / "local-0.patch").read_text()
                self.assertNotIn("diff --git", text)
                self.assertEqual(text.count("\n@@ -1 +1 @@\n"), 2)
                self.assertIn("+new\n--- a/nested/MODULE.bazel\n+++ b/nested/MODULE.bazel\n", text)
            if o["name"].startswith("native."):
                self.assertEqual(o["engine"], "native")

        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            code, report, seen = self.run_main(temporary, observe=observe)
        self.assertEqual(code, 0, [(c["name"], c.get("error")) for c in report["checks"] if c["status"] != "PASS"])
        self.assertEqual(report["status"], "PASS")
        self.assertEqual(report["completed_differential_cases"], 18)
        self.assertTrue(report["build_commands_executed"])
        self.assertFalse(report["test_commands_executed"])
        self.assertEqual(len({o["registry"] for o in seen}), 27)
        self.assertEqual(lexical_seen, {(name, engine) for name in (
            "graph.dot-prefix-is-not-root", "graph.empty-component-strip") for engine in ("native", "reference")})
        for name, version in (("graph.dot-prefix-is-not-root", "1.0"), ("graph.empty-component-strip", "2.0")):
            check = next(c for c in report["checks"] if c["name"] == name)
            self.assertEqual(check["expected_graph"]["dependencies"][0]["dependencies"][0]["version"], version)
            for index in check["run_ids"].values():
                graph = json.loads(report["runs"][index]["stdout"])
                self.assertEqual(graph["dependencies"][0]["dependencies"][0]["key"], "opencode_patchcheck_leaf@" + version)
        native_only = next(c for c in report["checks"] if c["name"] == "native.nonmodule-no-source")
        self.assertEqual(set(native_only["run_ids"]), {"native.result"})
        self.assertFalse(native_only["parity_claim"])
        self.assertTrue(native_only["source_json_absent"])
        boundary = next(c for c in report["checks"] if c["name"] == "graph.adjacent-plain-sections-skipped")
        self.assertEqual(set(boundary["run_ids"]), {"native.result", "reference.result"})
        self.assertTrue(boundary["parity_claim"])
        self.assertEqual(len({o["cwd"] for o in seen}), 1)
        roots = {arg for o in seen for arg in o["argv"] if arg.startswith("--output_base=")}
        self.assertEqual(len(roots), 1)
        self.assertEqual(sum(r["name"] == "patches.shutdown" for r in report["runs"]), 1)
        for check in report["checks"]:
            self.assertEqual(check["files_before"], check["files_after"])
            self.assertTrue(check["inputs_unchanged"] and check["command_marker_absent"])
        transformed = next(c for c in report["checks"] if c["name"] == "materialize.transform-order")
        self.assertEqual(set(transformed["artifacts"]), {"native.sources", "native.copy", "reference.sources", "reference.copy"})
        for engine in ("native", "reference"):
            artifact = transformed["artifacts"][engine + ".copy"]["copy.txt"]
            self.assertEqual(base64.b64decode(artifact["bytes_base64"]), "header\nlocal final café\nfooter\n".encode())
        cache = next(c for c in report["checks"] if c["name"] == "native.cache-local-patch-edit")
        self.assertNotEqual(cache["cache_root_before"], cache["cache_root_after"])
        self.assertIn(b"+edited final caf", base64.b64decode(cache["edited_patch"]["bytes_base64"]))

    def test_report_predicate_rejects_missing_forged_or_inconsistent_evidence(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            _, report, _ = self.run_main(temporary)
        self.assertTrue(patches.report_passes(report))
        for malformed in ({}, None, [], {"checks": []}, {"checks": "PASS"}):
            self.assertFalse(patches.report_passes(malformed))
        for mutation in ("missing", "duplicate", "kind", "parity", "status", "input", "files", "run-id",
                         "hash", "stdout", "scratch", "shutdown", "sources", "pin", "marker", "artifact",
                         "no-publication", "cache-key", "run-count", "descriptor", "archive"):
            changed = copy.deepcopy(report)
            first = changed["checks"][0]
            if mutation == "missing":
                changed["checks"].pop()
            elif mutation == "duplicate":
                changed["checks"][-1] = first
            elif mutation == "kind":
                first["kind"] = "native_only"
            elif mutation == "parity":
                changed["checks"][-1]["parity_claim"] = True
            elif mutation == "status":
                first["status"] = "UNSUPPORTED_BOUNDARY"
            elif mutation == "input":
                first["input"]["leaf"] = "1.0"
            elif mutation == "files":
                first["files_after"] = {}
            elif mutation == "run-id":
                first["run_ids"]["native.result"] = -1
            elif mutation == "hash":
                changed["runs"][1]["stdout_sha256"] = "wrong"
            elif mutation == "stdout":
                run = changed["runs"][first["run_ids"]["native.result"]]
                run["stdout"] = "{}"
                run["stdout_sha256"] = patches.digest(b"{}")
            elif mutation in ("scratch", "shutdown", "sources"):
                changed[{"scratch": "scratch_cleaned", "shutdown": "reference_shutdown_attempted", "sources": "sources_unchanged"}[mutation]] = False
            elif mutation == "pin":
                changed["executables"]["reference"]["sha256"] = "wrong"
            elif mutation == "marker":
                first["command_marker_absent"] = False
            elif mutation == "artifact":
                check = next(c for c in changed["checks"] if c["name"] == "materialize.transform-order")
                check["artifacts"]["reference.copy"]["copy.txt"]["bytes_base64"] = ""
            elif mutation == "no-publication":
                first["no_publication"] = False
            elif mutation == "cache-key":
                check = next(c for c in changed["checks"] if c["name"] == "native.cache-local-patch-edit")
                check["cache_root_after"] = check["cache_root_before"]
            elif mutation == "run-count":
                first["run_ids"].pop("reference.result")
            elif mutation == "descriptor":
                first["source_json_absent"] = True
            elif mutation == "archive":
                first["graph_archive_absent"] = False
            with self.subTest(mutation=mutation):
                self.assertFalse(patches.report_passes(changed))

    def test_failures_interrupts_and_mutations_always_cleanup_and_fail_closed(self):
        for fault in ("reference", "interrupt", "unexpected", "mutation", "marker", "shutdown", "version", "bytes", "publication"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
                code, report, _ = self.run_main(temporary, fault=fault)
            self.assertEqual(code, 130 if fault == "interrupt" else 1)
            self.assertEqual(report["status"], "FAIL")
            self.assertFalse(patches.report_passes(report))

    def test_cleanup_failure_is_recorded_not_a_pass(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            output = Path(temporary) / "report.json"
            original = patches.shutil.rmtree

            def cleanup(path):
                original(path)
                raise OSError("injected removal failure after cleanup")

            with mock.patch.object(patches, "validate_binaries", return_value={}), \
                    mock.patch.object(patches.Runner, "run", side_effect=RuntimeError("stop before real subprocess")), \
                    mock.patch.object(patches.shutil, "rmtree", side_effect=cleanup), mock.patch("builtins.print"):
                self.assertEqual(patches.main(["--scratch-parent", temporary, "--output", str(output)]), 1)
            report = json.loads(output.read_text())
            self.assertFalse(report["scratch_cleaned"])
            self.assertTrue(any("scratch cleanup" in error for error in report["errors"]))

    def test_missing_binary_and_protected_output_never_spawn(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temporary:
            output = Path(temporary) / "report.json"
            with mock.patch.object(patches.Runner, "run", side_effect=AssertionError("no subprocess")), mock.patch("builtins.print"):
                code = patches.main(["--native", temporary + "/absent", "--output", str(output)])
            report = json.loads(output.read_text())
            self.assertEqual(code, 1)
            self.assertEqual(report["runs"], [])
        for output in (patches.ROOT / "target/release/bazel-rs", Path(patches.__file__),
                       patches.ROOT / "tests/test_validate_patches.py", patches.ROOT / "tests/fixtures/new.json"):
            with self.subTest(output=output), mock.patch.object(patches, "validate_binaries") as binaries:
                with self.assertRaisesRegex(ValueError, "must not overwrite"):
                    patches.main(["--output", str(output)])
                binaries.assert_not_called()


if __name__ == "__main__":
    unittest.main()
