"""Offline tests of the pure parts of scripts/oss_corpus.py (config
validation, command lines, output parsing, comparisons, verdicts, the table,
workaround edits); no Bazel or bazel-rs binary and no network."""

import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import oss_corpus as oc

SHA = "0123456789abcdef0123456789abcdef01234567"


def write_config(projects):
    fd, path = tempfile.mkstemp(suffix=".json")
    with os.fdopen(fd, "w") as fh:
        json.dump({"projects": projects}, fh)
    return path


class ConfigTests(unittest.TestCase):
    def test_shipped_config_is_valid(self):
        cfg = oc.load_config(oc.DEFAULT_CONFIG)
        self.assertGreaterEqual(len(cfg["projects"]), 10)
        for p in cfg["projects"]:
            self.assertTrue(p["git"].startswith("https://github.com/"))

    def test_defaults(self):
        path = write_config([{"name": "a", "git": "https://github.com/x/a", "commit": SHA}])
        self.addCleanup(os.remove, path)
        p = oc.load_config(path)["projects"][0]
        self.assertEqual(p["targets"], ["//..."])
        self.assertEqual(p["test_targets"], ["//..."])
        self.assertEqual(p["subdir"], "")
        self.assertEqual(p["phases"], list(oc.PHASES))

    def test_rejects_short_commit_private_host_duplicates_and_bad_phase(self):
        bad = [
            [{"name": "a", "git": "https://github.com/x/a", "commit": "abc"}],
            [{"name": "a", "git": "https://example.com/x/a", "commit": SHA}],
            [{"name": "a", "git": "https://github.com/x/a", "commit": SHA}] * 2,
            [{"name": "a", "git": "https://github.com/x/a", "commit": SHA, "phases": ["fly"]}],
        ]
        for projects in bad:
            path = write_config(projects)
            self.addCleanup(os.remove, path)
            with self.assertRaises(ValueError):
                oc.load_config(path)

    def test_select(self):
        ps = [{"name": "a"}, {"name": "b"}]
        self.assertEqual(oc.select(ps, None), ps)
        self.assertEqual(oc.select(ps, "b,a"), [{"name": "b"}, {"name": "a"}])
        with self.assertRaises(ValueError):
            oc.select(ps, "c")


class ArgvTests(unittest.TestCase):
    def test_genuine_batch_and_server(self):
        argv = oc.tool_argv("genuine", "/g", "/w/p/genuine", "build", ["--nobuild"], ["//...", "-//x/..."])
        self.assertEqual(argv[0], "/g")
        self.assertIn("--output_user_root=/w/p/genuine/root", argv)
        self.assertIn("--install_base=/w/genuine-install", argv)
        self.assertIn("--host_jvm_args=-Duser.home=/w/p/genuine/home", argv)
        self.assertIn("--batch", argv)
        self.assertEqual(argv[-4:], ["--nobuild", "--", "//...", "-//x/..."])
        self.assertLess(argv.index("--batch"), argv.index("build"))
        server = oc.tool_argv("genuine", "/g", "/w/p/genuine", "shutdown", [], [], batch=False)
        self.assertNotIn("--batch", server)
        self.assertEqual(server[-1], "shutdown")

    def test_rs(self):
        argv = oc.tool_argv("rs", "/r", "/w/p/rs", "query", ["--keep_going"], ["//..."])
        self.assertEqual(argv, ["/r", "--nohome_rc", "--nosystem_rc", "--cache-dir=/w/p/rs/cache",
                                "query", "--keep_going", "--", "//..."])


class ParseTests(unittest.TestCase):
    def test_test_statuses(self):
        text = (
            "INFO: Build completed\n"
            "//a:t1                                PASSED in 0.3s\n"
            "\x1b[32m//a:t2\x1b[0m   (cached) PASSED in 0.1s\n"
            "//b:t3                                FAILED in 2.0s\n"
            "  /path/test.log\n"
            "//b:t4                       FAILED TO BUILD\n"
            "@@repo+//c:t5                         NO STATUS\n"
            "//d:t6                                SKIPPED\n"
            "Executed 2 out of 6 tests\n"
        )
        self.assertEqual(oc.parse_test_statuses(text), {
            "//a:t1": "PASSED", "//a:t2": "PASSED", "//b:t3": "FAILED",
            "//b:t4": "FAILED TO BUILD", "@@repo+//c:t5": "NO STATUS", "//d:t6": "SKIPPED",
        })

    def test_query_labels(self):
        self.assertEqual(oc.parse_query_labels("//b:x\n//a:y\n\nLoading: 3\n//a:y\n@r//:z\n"),
                         ["//a:y", "//b:x", "@r//:z"])

    def test_sample_paths_is_deterministic_and_spread(self):
        paths = [f"f{i:04d}" for i in range(1000)]
        sample = oc.sample_paths(list(reversed(paths)), 10)
        self.assertEqual(sample, [f"f{i:04d}" for i in range(0, 1000, 100)])
        self.assertEqual(oc.sample_paths(["b", "a"], 10), ["a", "b"])

    def test_normalize_manifest_drops_targets(self):
        a = b"_main/x /g/execroot/bin/x\n_repo_mapping /g/execroot/bin/x.repo_mapping\n"
        b = b"_repo_mapping /r/cache/actions/1/x.repo_mapping\n_main/x /r/cache/actions/2/x\n"
        self.assertEqual(oc.normalize_manifest(a), oc.normalize_manifest(b))
        self.assertNotEqual(oc.normalize_manifest(a), oc.normalize_manifest(b"_main/y /p\n"))

    def test_params_files_pattern(self):
        self.assertTrue(oc.PARAMS_RE.search("pkg/libx.so-2.params"))
        self.assertFalse(oc.PARAMS_RE.search("pkg/x.params.txt"))

    def test_integrity_to_sha256(self):
        self.assertEqual(
            oc.integrity_to_sha256("sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        with self.assertRaises(ValueError):
            oc.integrity_to_sha256("sha512-AAAA")


class CompareTests(unittest.TestCase):
    def test_compare_sets(self):
        c = oc.compare_sets(["a", "b", "c"], ["b", "c", "d", "e"])
        self.assertEqual((c["common"], c["only_genuine"], c["only_rs"]), (2, 1, 2))
        self.assertEqual(c["only_genuine_examples"], ["a"])
        self.assertEqual(c["only_rs_examples"], ["d", "e"])

    def test_compare_statuses(self):
        c = oc.compare_statuses({"//a": "PASSED", "//b": "PASSED"}, {"//a": "PASSED", "//b": "FAILED"})
        self.assertEqual(c["differences"], 1)
        self.assertEqual(c["examples"], {"//b": ("PASSED", "FAILED")})
        self.assertEqual(c["genuine"], {"PASSED": 2})
        self.assertEqual(c["rs"], {"FAILED": 1, "PASSED": 1})

    def entry(self, g, r, compare=None):
        return {
            "tools": {
                "genuine": {ph: {"exit": e} for ph, e in g.items()},
                "rs": {ph: {"exit": e} for ph, e in r.items()},
            },
            "compare": compare or {},
        }

    def test_verdicts(self):
        self.assertEqual(oc.verdict(self.entry({"build": 0}, {"build": 0})), "OK")
        self.assertEqual(oc.verdict(self.entry({"build": 0}, {"build": 1})), "RS-FAIL")
        self.assertEqual(oc.verdict(self.entry({"build": 1}, {"build": 1})), "BOTH-FAIL")
        self.assertEqual(oc.verdict(self.entry({"build": 1}, {"build": 0})), "DIFF")
        # The genuine-only server phase never counts.
        self.assertEqual(oc.verdict(self.entry({"server": 1, "build": 0}, {"build": 0})), "OK")
        # No test targets on either side is not a failure.
        self.assertEqual(oc.verdict(self.entry({"test": 4}, {"test": 4},
                                               {"tests": {"genuine": {}, "rs": {}, "differences": 0}})), "OK")
        outputs = {"outputs": {"common": 3, "only_genuine": 0, "only_rs": 0, "hash_mismatches": 1}}
        self.assertEqual(oc.verdict(self.entry({"build": 0}, {"build": 0}, outputs)), "DIFF")
        query = {"query": {"only_genuine": 2, "only_rs": 0}}
        self.assertEqual(oc.verdict(self.entry({"query": 0}, {"query": 0}, query)), "DIFF")

    def test_merge_result(self):
        old = {"commit": "c", "tools": {"genuine": {"build": {"exit": 0}}, "rs": {}}, "compare": {"outputs": 1}}
        new = {"commit": "c", "tools": {"genuine": {"test": {"exit": 0}}, "rs": {"test": {"exit": 3}}},
               "compare": {"tests": 2}}
        m = oc.merge_result(old, new)
        self.assertEqual(set(m["tools"]["genuine"]), {"build", "test"})
        self.assertEqual(m["compare"], {"outputs": 1, "tests": 2})
        self.assertIs(oc.merge_result(dict(old, commit="other"), new), new)

    def test_render_table(self):
        e = {
            "tools": {
                "genuine": {"nobuild_cold": {"exit": 0, "wall": 12.34, "rss_kb": 2048000},
                            "build": {"exit": 1, "wall": 3.0}},
                "rs": {"nobuild_cold": {"exit": 0, "wall": 5.0, "rss_kb": 204800}},
            },
            "compare": {"tests": {"genuine": {"PASSED": 2}, "rs": {"PASSED": 1, "FAILED": 1}, "differences": 1}},
        }
        table = oc.render_table({"p": e})
        row = table.splitlines()[2]
        self.assertIn("| p | DIFF | 12.3 / 5.0 |", row)
        self.assertIn("3.0 (exit 1) / —", row)
        self.assertIn("2000 / 200", row)
        self.assertIn("PASSED:2 / FAILED:1 PASSED:1", row)


class EditTests(unittest.TestCase):
    def test_comment_out(self):
        text = "startup --host_jvm_args=x\n# --module_mirrors=y\ncommon --keep\ncommon --module_mirrors=z\n"
        out = oc.comment_out(text, ["--host_jvm_args", "--module_mirrors"])
        self.assertEqual(out.splitlines(), [
            "# oss_corpus workaround: startup --host_jvm_args=x",
            "# --module_mirrors=y",
            "common --keep",
            "# oss_corpus workaround: common --module_mirrors=z",
        ])

    def test_apply_edits(self):
        with tempfile.TemporaryDirectory() as ws:
            Path(ws, "MODULE.bazel").write_text('bazel_dep(name = "a", version = "1")\n')
            Path(ws, ".bazelrc").write_text("startup --windows_enable_symlinks\nbuild --x\n")
            Path(ws, "MODULE.bazel.lock").write_text("{}")
            project = {
                "name": "p",
                "delete": ["MODULE.bazel.lock", "absent"],
                "replace": [{"file": "MODULE.bazel", "old": '"1"', "new": '"2"'}],
                "comment_out": [{"file": ".bazelrc", "lines": ["windows_enable_symlinks"]}],
            }
            oc.apply_edits(project, ws, ws)
            self.assertFalse(Path(ws, "MODULE.bazel.lock").exists())
            self.assertIn('"2"', Path(ws, "MODULE.bazel").read_text())
            self.assertTrue(Path(ws, ".bazelrc").read_text().startswith("# oss_corpus workaround: startup"))
            with self.assertRaises(ValueError):
                oc.apply_edits({"name": "p", "replace": [{"file": "MODULE.bazel", "old": "zzz", "new": ""}]}, ws, ws)


class ListOutputsTests(unittest.TestCase):
    def test_follows_directory_links_skips_runfiles_and_cycles(self):
        with tempfile.TemporaryDirectory() as root:
            Path(root, "a/b").mkdir(parents=True)
            Path(root, "a/b/f").write_text("x")
            Path(root, "bin.runfiles/_main").mkdir(parents=True)
            Path(root, "bin.runfiles/_main/f").write_text("x")
            Path(root, "bin").write_text("x")
            os.symlink("../a", Path(root, "a/b/loop"))  # cycle into an ancestor
            os.symlink("a/b", Path(root, "tree"))  # a linked tree artifact
            os.symlink("missing", Path(root, "dangling"))
            self.assertEqual(oc.list_outputs(root), ["a/b/f", "bin", "tree/f"])


if __name__ == "__main__":
    unittest.main()
