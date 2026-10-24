"""Offline tests of scripts/ab_bench.py: output normalisation, PERF parsing
and the --compare-only report over synthetic run files (no binary runs)."""

import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import ab_bench as ab


CACHE = "/tmp/opencode/x-ab/cache-a"

ERR = f"""bazel-rs: memory limit defaulted to 65536 MB
PERF     0.000s rss=19448kB main.start hwm=19448kB
ACTION a1 GoCompilePkg
  input {CACHE}/repositories/direct/trees/0123456789abcdef0123/x.go -> external/x/x.go
  output bazel-out/k8-fastbuild/bin/x.a
PERF-STAGE total prepare 17.125s count=18968
PERF-STAGE total plan_span 46.100s covered=97.1%
PERF-STAGE flush-batch flushes=3 planned=40 empty=1 min=0 median=8 p90=32 max=32 phase2=1200
PERF-STAGE views edges=900 constructed=900 distinct=300 artifact_file=12000
PERF-ALLOC 1024 action_planning.provider_files
PERF-ALLOC 512 shared:DepsetNode
PERF-ALLOC-DETAIL 256 plan/part:schema
PERF-ALLOC-HEAP 2048 custom_providers heaps=12
PERF-ALLOC-COUNTS packages=2 rules=9
PERF-ALLOC-SUMMARY total=1536 rss=102400kB hwm=204800kB covered=1.5% rss_anon=1kB rss_file=2kB threads=2 elapsed=0.250s
--native-lockfile=/tmp/opencode/x-ab/lock-a ok
PERF    45.500s rss=1400000kB plan.end hwm=1450000kB
INFO: done
"""


class NormalisationTests(unittest.TestCase):
    def test_norm_replaces_cache_side_and_tree_hashes(self):
        text = f"{CACHE}/repositories/direct/trees/deadbeefcafe0123/BUILD /x/lock-b /x/lock-bb"
        self.assertEqual(
            ab.norm(text, CACHE),
            "<CACHE>/repositories/direct/trees/<RUN>/BUILD /x/lock-<SIDE> /x/lock-bb",
        )

    def test_split_err_separates_dump_rest_and_perf(self):
        dump, rest, perf = ab.split_err(ERR, CACHE)
        self.assertEqual(dump, [
            "ACTION a1 GoCompilePkg",
            "  input <CACHE>/repositories/direct/trees/<RUN>/x.go -> external/x/x.go",
            "  output bazel-out/k8-fastbuild/bin/x.a",
        ])
        self.assertEqual(rest, [
            "bazel-rs: memory limit defaulted to 65536 MB",
            "--native-lockfile=/tmp/opencode/x-ab/lock-<SIDE> ok",
            "INFO: done",
        ])
        self.assertTrue(all(line.startswith("PERF") for line in perf))
        self.assertEqual(len(perf), 12)

    def test_perf_marks_with_and_without_hwm(self):
        marks = ab.perf_marks([
            "PERF     0.000s rss=19448kB main.start hwm=19448kB",
            "PERF    12.250s rss=2048kB plan.start",  # pre-W0-A binary
            "PERF-STAGE total prepare 1.000s count=1",
            "PERF modules: evaluated=1 waits=0 hits=2 self_total=0.1s enter_total=0.2s",
        ])
        self.assertEqual(marks, {
            "main.start": {"t": 0.0, "rss_kb": 19448, "rss_mb": 18, "hwm_kb": 19448},
            "plan.start": {"t": 12.25, "rss_kb": 2048, "rss_mb": 2, "hwm_kb": None},
        })

    def test_stage_totals_and_counters(self):
        _, _, perf = ab.split_err(ERR, CACHE)
        self.assertEqual(ab.stage_totals(perf), {
            "prepare": {"s": 17.125, "count": 18968},
            "plan_span": {"s": 46.1, "covered_pct": 97.1},
        })
        rows = ab.perf_counters(perf)
        self.assertEqual(rows["flush_batch"], {
            "flushes": 3, "planned": 40, "empty": 1, "min": 0, "median": 8, "p90": 32,
            "max": 32, "phase2": 1200})
        self.assertEqual(rows["views"]["distinct"], 300)
        self.assertEqual(rows["alloc_summary"]["covered"], 1.5)
        self.assertEqual(rows["alloc_summary"]["rss"], 102400)
        self.assertEqual(ab.alloc_rows(perf), [
            {"bytes": 1024, "name": "action_planning.provider_files"},
            {"bytes": 512, "name": "shared:DepsetNode"},
        ])
        self.assertEqual(ab.alloc_rows(perf, "-DETAIL"), [{"bytes": 256, "name": "plan/part:schema"}])
        self.assertEqual(ab.alloc_rows(perf, "-HEAP"),
                         [{"bytes": 2048, "name": "custom_providers", "heaps": 12}])
        self.assertEqual(rows["alloc_counts"], {"packages": 2, "rules": 9})
        self.assertEqual(rows["alloc_summary"]["threads"], 2)

    def test_summarize_takes_medians(self):
        records = [
            {"wall": 10.0, "user": 9.0, "sys": 1.0, "cpu": 10.0, "maxrss_kb": 100,
             "marks": {"plan.end": {"t": 9.0, "rss_kb": 1, "hwm_kb": 5}}, "stages": {"prepare": {"s": 3.0}}},
            {"wall": 30.0, "user": 27.0, "sys": 3.0, "cpu": 30.0, "maxrss_kb": 300,
             "marks": {"plan.end": {"t": 29.0, "rss_kb": 3, "hwm_kb": 7}}, "stages": {"prepare": {"s": 9.0}}},
            {"wall": 20.0, "user": 18.0, "sys": 2.0, "cpu": 20.0, "maxrss_kb": 200,
             "marks": {}, "stages": {}},
        ]
        summary = ab.summarize(records)
        self.assertEqual(summary["runs"], 3)
        self.assertEqual(summary["wall"], 20.0)
        self.assertEqual(summary["cpu"], 20.0)
        self.assertEqual(summary["maxrss_kb"], 200)
        # Missing values are skipped, not counted as zero.
        self.assertEqual(summary["marks"]["plan.end"], {"t": 19.0, "rss_kb": 2.0, "hwm_kb": 6.0})
        self.assertEqual(summary["stages_s"], {"prepare": 6.0})


class CompareOnlyTests(unittest.TestCase):
    def write_run(self, out, tag, cache, err, stdout="//x:y\n", rc=0, wall=1.0):
        (out / f"{tag}.err").write_text(err.replace(CACHE, cache))
        (out / f"{tag}.out").write_text(stdout)
        (out / f"{tag}.json").write_text(json.dumps({"rc": rc, "wall": wall, "user": wall, "sys": 0.0,
                                                     "cpu": wall, "maxrss_kb": 10}))

    def caches(self, root):
        return {"a": str(root / "cache-a"), "b": str(root / "cache-b")}

    def test_equivalent_runs_differ_only_in_noise(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            caches = self.caches(out)
            self.write_run(out, "a-0", caches["a"], ERR)
            self.write_run(out, "b-0", caches["b"], ERR.replace("0123456789abcdef0123", "fedcba9876543210")
                           .replace("lock-a", "lock-b").replace("45.500s", "12.000s"))
            report = ab.build_report(out, {}, caches, ["a", "b"], 0, {})
            self.assertTrue(report["equivalent"], report["compare"])
            self.assertEqual(report["compare"]["dump_actions"], {"a": 1, "b": 1})
            self.assertEqual(report["summary"]["b"]["marks"]["plan.end"]["t"], 12.0)
            self.assertEqual(report["runs"]["a"][0]["peak_hwm_kb"], 1450000)
            self.assertFalse(list(out.glob("diff-*")))

    def test_dump_difference_is_reported_with_diff_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            caches = self.caches(out)
            self.write_run(out, "a-0", caches["a"], ERR)
            self.write_run(out, "a-1", caches["a"], ERR)
            self.write_run(out, "b-0", caches["b"], ERR)
            self.write_run(out, "b-1", caches["b"], ERR.replace("output bazel-out", "output bazel-OUT"))
            report = ab.build_report(out, {}, caches, ["a", "b"], 1, {})
            compare = report["compare"]
            self.assertEqual(compare["compared"], {"a": "a-1", "b": "b-1"})
            self.assertFalse(compare["dump_equal"])
            self.assertTrue(compare["stderr_rest_equal"])
            self.assertFalse(report["equivalent"])
            self.assertTrue((out / "diff-dump.a").exists() and (out / "diff-dump.b").exists())
            # One warm-up excluded from the medians.
            self.assertEqual(report["summary"]["a"]["runs"], 1)

    def test_rc_from_an_ab_equiv_report_and_main_compare_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            caches = self.caches(out)
            for side in "ab":
                (out / f"{side}-0.err").write_text(ERR.replace(CACHE, caches[side]))
                (out / f"{side}-0.out").write_text("ok\n")
            # An ab_equiv.py report: runs by index, no per-run json.
            (out / "report.json").write_text(json.dumps({
                "runs": {"a": [{"rc": 0, "wall": 3.0}], "b": [{"rc": 1, "wall": 4.0}]},
                "canary_unchanged": True}))
            with self.assertRaises(SystemExit) as exit_code, contextlib.redirect_stdout(io.StringIO()):
                ab.main(["--cache-a", caches["a"], "--cache-b", caches["b"], "--out", str(out),
                         "--compare-only"])
            self.assertEqual(exit_code.exception.code, 1)
            report = json.loads((out / "report.json").read_text())
            self.assertFalse(report["compare"]["rc_equal"])
            self.assertEqual(report["runs"]["b"][0]["wall"], 4.0)
            self.assertTrue(report["canary_unchanged"])

    def test_lockfile_mode_is_never_off_by_default(self):
        # Integration-20 (W0-A review fix 7): without a seed, only an
        # explicit --lockfile-mode off runs with --lockfile_mode=off.
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            caches = self.caches(out)
            common = ["--workspace", str(out / "ws"), "--a", "/bin/false", "--b", "/bin/false",
                      "--cache-a", caches["a"], "--cache-b", caches["b"], "--out", str(out / "out")]
            for extra, needle in (([], "--lockfile-mode off"),
                                  (["--lockfile-mode", "update"], "needs --lockfile-seed"),
                                  (["--lockfile-seed", str(out / "seed"), "--lockfile-mode", "off"],
                                   "contradict")):
                with self.assertRaises(SystemExit) as refused:
                    ab.main(common + extra + ["//:x"])
                self.assertIn(needle, str(refused.exception.code))
            self.assertFalse((out / "out").exists())
            explicit = ab.argparse.Namespace(lockfile_seed=None, lockfile_mode="off", lockfile_dir=None,
                                             mode="nobuild", extra_arg=[], targets=["//:x"])
            self.assertIn("--lockfile_mode=off", ab.side_args(explicit, out, "a"))
            silent = ab.argparse.Namespace(lockfile_seed=None, lockfile_mode=None, lockfile_dir=None,
                                           mode="nobuild", extra_arg=[], targets=["//:x"])
            with self.assertRaises(ValueError):
                ab.side_args(silent, out, "a")
            (out / "seed").write_text("seed")
            seeded = ab.argparse.Namespace(lockfile_seed=str(out / "seed"), lockfile_mode=None,
                                           lockfile_dir=None, mode="build", extra_arg=[], targets=["//:x"])
            self.assertIn("--lockfile_mode=update", ab.side_args(seeded, out, "b"))
            self.assertEqual((out / "lock-b").read_text(), "seed")

    def test_refuses_caches_inside_the_workspace(self):
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            with self.assertRaises(SystemExit) as refused:
                ab.main(["--workspace", str(workspace), "--cache-a", str(workspace / "bazel-rs-out"),
                         "--cache-b", "/tmp/elsewhere", "--out", "/tmp/elsewhere-out", "--compare-only"])
            self.assertIn("inside the workspace", str(refused.exception.code))

    def test_relative_paths_are_absolute_before_any_use(self):
        # The binaries run with cwd=<workspace>; relative paths must be resolved against the
        # harness's cwd first, or caches/HOME/lockfiles would land inside the workspace.
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp).resolve()
            workspace = base / "ws"
            workspace.mkdir()
            harness = base / "h"
            harness.mkdir()
            before = os.getcwd()
            os.chdir(harness)
            try:
                args = ab.absolutize(ab.argparse.Namespace(
                    a="bin-a", b=None, cache_a="ca", cache_b="cb", out="out", lockfile_seed=None,
                    lockfile_dir="locks", workspace=str(workspace)))
                self.assertEqual(args.cache_a, str(harness / "ca"))
                self.assertEqual(args.out, str(harness / "out"))
                self.assertEqual(args.lockfile_dir, str(harness / "locks"))
                self.assertEqual(args.a, str(harness / "bin-a"))
                self.assertIsNone(args.b)
                for extra in ([], ["--lockfile-dir", "../ws/locks"]):
                    cache = "../ws/cache" if not extra else "ca"
                    with self.assertRaises(SystemExit) as refused:
                        ab.main(["--workspace", str(workspace), "--cache-a", cache, "--cache-b", "cb",
                                 "--out", "out", "--compare-only"] + extra)
                    self.assertIn("inside the workspace", str(refused.exception.code))
                self.assertEqual(sorted(p.name for p in workspace.iterdir()), [])
            finally:
                os.chdir(before)


if __name__ == "__main__":
    unittest.main()
