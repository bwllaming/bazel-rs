"""Offline capture integrity/projection tests; no simulated parity claims."""

import copy
import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import capture_builtin_modules as capture


FIXTURE = Path(__file__).resolve().parent / "fixtures/builtin_modules"


class CaptureProjectionTests(unittest.TestCase):
    def setUp(self):
        self.expectations = json.loads((FIXTURE / "expectations.json").read_bytes())
        self.graph = json.loads((FIXTURE / self.expectations["cases"][0]["reference_graph"]).read_bytes())
        self.lock = json.loads((FIXTURE / self.expectations["cases"][0]["reference_lockfile"]).read_bytes())

    def test_recorded_oracles_project_exact_selected_versions(self):
        for case in self.expectations["cases"]:
            graph = json.loads((FIXTURE / case["reference_graph"]).read_bytes())
            self.assertEqual(capture.flatten_graph(graph), case["selected"])
            selected = {m["name"]: m["version"] for m in case["selected"]}
            self.assertEqual(selected["bazel_tools"], "")
            self.assertEqual(selected["platforms"], "1.0.0")
            self.assertNotIn("opencode_builtin_fixture", selected)

    def test_real_cycles_and_unexpanded_nodes_do_not_duplicate_selection(self):
        text = json.dumps(self.graph)
        self.assertIn('"unexpanded": true', text)
        self.assertIn('"cycles": [{', text)
        projected = capture.flatten_graph(self.graph)
        self.assertEqual(len(projected), len({m["name"] for m in projected}))

    def test_projection_rejects_bad_root_and_missing_builtin(self):
        wrong_root = copy.deepcopy(self.graph)
        wrong_root["name"] = "wrong"
        with self.assertRaises(ValueError):
            capture.flatten_graph(wrong_root)
        missing = {**self.graph, "dependencies": [], "cycles": [], "indirectDependencies": []}
        with self.assertRaises(ValueError):
            capture.flatten_graph(missing)

    def test_projection_rejects_conflicting_identity_and_unused_node(self):
        child = copy.deepcopy(self.graph["dependencies"][0])
        child["version"] = "not-the-official-version"
        conflict = copy.deepcopy(self.graph)
        conflict["dependencies"].append(child)
        with self.assertRaisesRegex(ValueError, "multiple selected versions"):
            capture.flatten_graph(conflict)
        child["unused"] = True
        with self.assertRaisesRegex(ValueError, "unused"):
            capture.flatten_graph(conflict)

    def test_discovery_includes_real_unselected_versions(self):
        plan = capture.registry_plan([self.lock])
        self.assertIn(capture.BCR + "/modules/platforms/0.0.10/MODULE.bazel", plan)
        selected = self.expectations["cases"][0]["selected"]
        self.assertNotIn({"name": "platforms", "version": "0.0.10"}, selected)
        self.assertGreater(sum(url.endswith("/MODULE.bazel") for url in plan), len(selected))

    def test_plan_includes_selected_and_unselected_names_metadata(self):
        plan = capture.registry_plan([self.lock])
        for url in plan:
            if url.endswith("/MODULE.bazel"):
                name = capture.registry_path(url).split("/")[1]
                self.assertIn(capture.BCR + "/modules/" + name + "/metadata.json", plan)

    def test_plan_rejects_missing_or_invalid_hashes(self):
        for value in (None, "", "not-a-hash", "A" * 64):
            bad = copy.deepcopy(self.lock)
            first = next(iter(bad["registryFileHashes"]))
            bad["registryFileHashes"][first] = value
            with self.subTest(value=value), self.assertRaises(ValueError):
                capture.registry_plan([bad])
        for bad in ({}, {"registryFileHashes": {}}, {"registryFileHashes": []}):
            with self.assertRaises(ValueError):
                capture.registry_plan([bad])

    def test_plan_rejects_config_hole_and_case_hash_conflict(self):
        bad = copy.deepcopy(self.lock)
        del bad["registryFileHashes"][capture.BCR + "/bazel_registry.json"]
        with self.assertRaisesRegex(ValueError, "configuration"):
            capture.registry_plan([bad])
        bad = copy.deepcopy(self.lock)
        first = next(iter(bad["registryFileHashes"]))
        bad["registryFileHashes"][first] = "0" * 64
        with self.assertRaisesRegex(ValueError, "conflicting"):
            capture.registry_plan([self.lock, bad])

    def test_plan_rejects_untrusted_sources_and_unsafe_paths(self):
        for url in ("http://bcr.bazel.build/bazel_registry.json",
                    "https://bcr.bazel.build.evil.test/bazel_registry.json",
                    capture.BCR + "/modules/m/../MODULE.bazel",
                    capture.BCR + "/modules/m/%2e%2e/MODULE.bazel",
                    capture.BCR + "/modules/m/1/MODULE.bazel?alternate=1",
                    "file:///tmp/local/MODULE.bazel"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                capture.registry_path(url)

    def test_hash_validation_preserves_exact_bytes(self):
        url = capture.BCR + "/bazel_registry.json"
        data = (FIXTURE / "registry/bazel_registry.json").read_bytes()
        self.assertEqual(capture.validate_bytes(url, data, capture.digest(data)), capture.digest(data))
        with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
            capture.validate_bytes(url, data + b"\n", capture.digest(data))
        with self.assertRaises(ValueError):
            capture.validate_bytes(url, b"[]", None)

    def test_timeouts_and_reference_failures_never_count_as_success(self):
        for code, timeout in ((0, True), (1, False), (-9, False), (None, False)):
            record = {"name": "oracle", "id": 0, "returncode": code,
                      "timed_out": timeout, "stdout": json.dumps(self.graph)}
            with self.subTest(code=code, timeout=timeout), self.assertRaises(ValueError):
                capture.require_success(record)


class CapturedFixtureIntegrityTests(unittest.TestCase):
    def test_complete_mirror_matches_all_official_lockfile_hashes(self):
        counts = capture.verify_fixture(FIXTURE)
        self.assertGreater(counts["requested_module_count"], 0)
        self.assertGreater(counts["metadata_count"], 0)

    def test_deleted_unselected_module_is_a_plan_hole(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temp:
            fixture = Path(temp) / "fixture"
            shutil.copytree(FIXTURE, fixture)
            (fixture / "registry/modules/platforms/0.0.10/MODULE.bazel").unlink()
            with self.assertRaisesRegex(ValueError, "missing or extra"):
                capture.verify_fixture(fixture)

    def test_missing_case_and_rewritten_selection_are_not_valid_captures(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temp:
            fixture = Path(temp) / "fixture"
            shutil.copytree(FIXTURE, fixture)
            path = fixture / "expectations.json"
            original = json.loads(path.read_bytes())
            changed = copy.deepcopy(original)
            changed["cases"].pop()
            capture.save_json(path, changed)
            with self.assertRaisesRegex(ValueError, "oracle case"):
                capture.verify_fixture(fixture)
            changed = copy.deepcopy(original)
            changed["cases"][0]["selected"].pop()
            capture.save_json(path, changed)
            with self.assertRaisesRegex(ValueError, "projection"):
                capture.verify_fixture(fixture)

    def test_supplemental_patches_are_exactly_the_source_json_pinned_genuine_files(self):
        counts = capture.verify_fixture(FIXTURE)
        self.assertEqual(counts["registry_patch_count"], 7)
        provenance = json.loads((FIXTURE / "provenance.json").read_bytes())
        on_disk = sorted(str(p.relative_to(FIXTURE)) for p in (FIXTURE / "registry").rglob("patches/*"))
        self.assertEqual(sorted(provenance["registry_patches"]), on_disk)
        self.assertNotIn("registry_patches", json.dumps(provenance["files"]))
        self.assertTrue(all("/patches/" not in path for path in provenance["files"]))
        shell = "registry/modules/rules_shell/0.6.1/patches/module_dot_bazel_version.patch"
        self.assertEqual(provenance["registry_patches"][shell]["sha256"],
                         "5f0700eaa9a33770aae4ae8b06bec8e433f518eb50711378c8cd3a5d7854ff2d")
        self.assertIn(b'-    version = "0.0.0",\n+    version = "0.6.1",\n', (FIXTURE / shell).read_bytes())

    def test_unpinned_changed_or_undeclared_patch_files_are_rejected(self):
        shell = "registry/modules/rules_shell/0.6.1/patches/module_dot_bazel_version.patch"
        for fault in ("unlisted", "changed", "missing-file", "undeclared", "wrong-url", "extra-key"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(dir="/tmp/opencode") as temp:
                fixture = Path(temp) / "fixture"
                shutil.copytree(FIXTURE, fixture)
                path = fixture / "provenance.json"
                provenance = json.loads(path.read_bytes())
                entry = provenance["registry_patches"][shell]
                if fault == "unlisted":
                    del provenance["registry_patches"][shell]
                elif fault == "changed":
                    (fixture / shell).write_bytes((fixture / shell).read_bytes() + b"\n")
                elif fault == "missing-file":
                    (fixture / shell).unlink()
                elif fault == "undeclared":
                    other = shell.replace("module_dot_bazel_version.patch", "other.patch")
                    shutil.copy(fixture / shell, fixture / other)
                    provenance["registry_patches"][other] = dict(entry, source_url=entry["source_url"].replace(
                        "module_dot_bazel_version.patch", "other.patch"))
                elif fault == "wrong-url":
                    entry["source_url"] = entry["source_url"].replace("0.6.1", "0.3.0")
                else:
                    entry["extra"] = True
                capture.save_json(path, provenance)
                with self.assertRaises((ValueError, OSError)):
                    capture.verify_fixture(fixture)

    def test_rewritten_metadata_is_rejected_even_without_lockfile_hash(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as temp:
            fixture = Path(temp) / "fixture"
            shutil.copytree(FIXTURE, fixture)
            path = fixture / "registry/modules/platforms/metadata.json"
            path.write_bytes(path.read_bytes() + b"\n")
            with self.assertRaisesRegex(ValueError, "provenance mismatch"):
                capture.verify_fixture(fixture)


if __name__ == "__main__":
    unittest.main()
