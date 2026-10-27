"""Offline tests of the pure parts of scripts/bench_genuine.py (statistics,
BEP / stdout output parsing, path normalisation, test summaries, reports);
no Bazel or bazel-rs binary runs."""

import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import bench_genuine as bg


class StatsTests(unittest.TestCase):
    def test_median_odd_and_even(self):
        self.assertEqual(bg.median([3, 1, 2]), 2)
        self.assertEqual(bg.median([4, 1, 3, 2]), 2.5)

    def test_percentile_is_nearest_rank(self):
        samples = [5, 1, 4, 2, 3]
        self.assertEqual(bg.percentile(samples, 95), 5)
        self.assertEqual(bg.percentile(samples, 50), 3)
        self.assertEqual(bg.percentile(list(range(1, 21)), 95), 19)
        self.assertEqual(bg.percentile([7], 95), 7)

    def test_empty_samples_raise(self):
        with self.assertRaises(ValueError):
            bg.median([])
        with self.assertRaises(ValueError):
            bg.percentile([], 95)

    def test_summarize(self):
        self.assertEqual(
            bg.summarize([1.0, 3.0, 2.0]),
            {"n": 3, "median": 2.0, "p95": 3.0, "min": 1.0, "max": 3.0},
        )


class OrderAndEditTests(unittest.TestCase):
    def test_rotation_spreads_the_order(self):
        sides = ("gs", "gb", "rs")
        self.assertEqual(bg.rotated(sides, 0), ["gs", "gb", "rs"])
        self.assertEqual(bg.rotated(sides, 1), ["gb", "rs", "gs"])
        self.assertEqual(bg.rotated(sides, 3), ["gs", "gb", "rs"])
        self.assertEqual(bg.rotated((), 2), [])

    def test_edit_text_substitutes_the_counter(self):
        self.assertEqual(bg.edit_text("\nvar benchEdit{n} = {n}\n", 7), "\nvar benchEdit7 = 7\n")


class PathTests(unittest.TestCase):
    def test_labels(self):
        self.assertEqual(bg.normalize_label("@@//pkg:t"), "//pkg:t")
        self.assertEqual(bg.normalize_label("@//:t"), "//:t")
        self.assertEqual(bg.normalize_label("//a:b"), "//a:b")
        self.assertEqual(bg.normalize_label("@@rules_go+//go:x"), "@@rules_go+//go:x")

    def test_relative_output_paths(self):
        ws = "/w/ws-gs"
        self.assertEqual(
            bg.relative_output_path("file:///w/root/abc/execroot/_main/bazel-out/k8-fastbuild/bin/x", ws),
            "bazel-out/k8-fastbuild/bin/x",
        )
        self.assertEqual(
            bg.relative_output_path("/w/cache/actions/ff/outputs/bazel-out/k8-fastbuild/bin/x", "/w/ws-rs"),
            "bazel-out/k8-fastbuild/bin/x",
        )
        self.assertEqual(bg.relative_output_path("/w/ws-gs/pkg/main.py", ws), "pkg/main.py")
        self.assertEqual(bg.relative_output_path("/elsewhere/f", ws), "/elsewhere/f")
        self.assertEqual(
            bg.relative_output_path("/w/root/abc/external/bazel_skylib+/lib/paths.bzl", ws),
            "external/*/lib/paths.bzl",
        )
        self.assertEqual(
            bg.relative_output_path("/w/cache/repositories/trees/44ff/lib/paths.bzl", "/w/ws-rs"),
            "external/*/lib/paths.bzl",
        )
        self.assertEqual(
            bg.relative_output_path("/w/cache/repositories/direct/trees/44ff/x/y.go", "/w/ws-rs"),
            "external/*/x/y.go",
        )


BEP = [
    {"id": {"namedSet": {"id": "0"}}, "namedSetOfFiles": {"files": [{"name": "a", "uri": "file:///r/execroot/_main/bazel-out/bin/a"}], "fileSets": [{"id": "1"}]}},
    {"id": {"namedSet": {"id": "1"}}, "namedSetOfFiles": {"files": [{"name": "b", "uri": "file:///ws/b.py"}]}},
    {"id": {"targetCompleted": {"label": "//:t"}}, "completed": {"success": True, "outputGroup": [{"name": "default", "fileSets": [{"id": "0"}]}, {"name": "_hidden_top_level_INTERNAL_", "fileSets": [{"id": "9"}]}]}},
    {"id": {"targetCompleted": {"label": "@@//:u"}}, "completed": {"success": True, "outputGroup": [{"name": "default", "fileSets": [{"id": "1"}]}]}},
    {"id": {"progress": {}}, "progress": {}},
]


class OutputParsingTests(unittest.TestCase):
    def test_bep_default_group_with_nested_sets(self):
        got = bg.parse_bep_outputs([json.dumps(e) for e in BEP] + [""])
        self.assertEqual(
            got,
            {
                "//:t": ["file:///r/execroot/_main/bazel-out/bin/a", "file:///ws/b.py"],
                "//:u": ["file:///ws/b.py"],
            },
        )

    def test_bazel_rs_stdout(self):
        text = "//:t\t/c/actions/1/outputs/bazel-out/bin/a\n//:t\t/ws/b.py\nnoise line\nINFO: x\t y\n"
        self.assertEqual(
            bg.parse_rs_outputs(text),
            {"//:t": ["/c/actions/1/outputs/bazel-out/bin/a", "/ws/b.py"]},
        )

    def test_compare_outputs(self):
        a = {"//:t": {"x": "1", "y": "2"}, "//:only_a": {}}
        b = {"//:t": {"x": "1", "y": "3", "z": "4"}, "//:only_b": {}}
        self.assertEqual(
            bg.compare_outputs(a, b),
            [
                "//:only_a: only in first",
                "//:only_b: only in second",
                "//:t: y content differs",
                "//:t: z only in second",
            ],
        )
        self.assertEqual(bg.compare_outputs(a, a), [])

    def test_sha256_of_files_and_directories(self):
        with tempfile.TemporaryDirectory() as tmp:
            f = os.path.join(tmp, "f")
            Path(f).write_bytes(b"abc")
            self.assertEqual(
                bg.sha256_path(f),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            )
            d = os.path.join(tmp, "d")
            os.makedirs(os.path.join(d, "s"))
            Path(d, "s", "g").write_bytes(b"abc")
            first = bg.sha256_path(d)
            self.assertTrue(first.startswith("dir:"))
            Path(d, "s", "g").write_bytes(b"abd")
            self.assertNotEqual(bg.sha256_path(d), first)
            self.assertTrue(bg.sha256_path(os.path.join(tmp, "missing")).startswith("error:"))


class TestSummaryTests(unittest.TestCase):
    def test_parse(self):
        text = (
            "//:a_test                     (cached) PASSED in 0.1s\n"
            "@@//pkg:b_test                FAILED in 2.0s\n"
            "  /path/to/test.log\n"
            "\n"
            "Executed 1 out of 2 tests: 1 test passes and 1 fails locally.\n"
        )
        statuses, summary = bg.parse_test_summary(text)
        self.assertEqual(statuses, {"//:a_test": "PASSED", "//pkg:b_test": "FAILED"})
        self.assertEqual(summary, "Executed 1 out of 2 tests: 1 test passes and 1 fails locally.")


class ReportTests(unittest.TestCase):
    def test_report_tables_and_correctness_notes(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = os.path.join(tmp, "demo")
            os.makedirs(os.path.join(repo, "runs", "query"))
            rows = []
            for side, walls, code in (("gs", [0.2, 0.1, 0.3], 0), ("rs", [0.5, 0.4, 0.6], 1)):
                for rep, wall in enumerate(walls):
                    rows.append({
                        "repo": "demo", "scenario": "query", "side": side, "rep": rep,
                        "measured": True, "wall_s": wall, "exit": code if rep == 2 else 0,
                        "client_maxrss_kb": 2048, "rs_hwm_kb": 4096 if side == "rs" else None,
                        "server_hwm_kb": 10240 if side == "gs" else None, "stem": f"{side}-{rep}",
                    })
                    Path(repo, "runs", "query", f"{side}-{rep}.out").write_text("//:b\n//:a\n" if side == "gs" else "//:a\n//:b\n")
            rows.append(dict(rows[0], measured=False, wall_s=99.0))
            Path(repo, "results.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
            Path(repo, "verify.json").write_text(json.dumps({
                "gs_vs_rs": [], "gs_vs_gb": [], "labels": {"gs": 2}, "files": {"gs": 3}}))
            text = bg.build_report(tmp)
        self.assertIn("| query | gs | 3 | 0.20 | 0.30 | 0.10 | 0.30 | 12 | 0 | 0.20 0.10 0.30 |", text)
        self.assertIn("| query | rs | 3 | 0.50 | 0.60 | 0.40 | 0.60 | 4 | 1 | 0.50 0.40 0.60 |", text)
        self.assertNotIn("99.00", text)
        self.assertIn("demo query rep 2: exit codes differ {'gs': 0, 'rs': 1}", text)
        self.assertIn("demo query output gs vs rs: identical (as sets)", text)
        self.assertIn("demo outputs gs vs rs: 0 differences", text)

    def test_render_table(self):
        self.assertEqual(bg.render_table([[1, "x"]], ["a", "b"]), "| a | b |\n|---|---|\n| 1 | x |")


if __name__ == "__main__":
    unittest.main()
