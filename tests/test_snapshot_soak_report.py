"""Offline tests of scripts/snapshot_soak_report.py (the soak's event and verify logs, L2-W5h)."""

import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from contextlib import redirect_stdout

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import snapshot_soak_report as report


def line(seconds, kind, key, mode="on", command="build", reason="-", path="-", ms="-", detail="-",
         targets="//a:b"):
    return (f"2026-10-24T00:00:{seconds % 60:02d}Z {1792800000 + seconds} kind={kind} mode={mode} key={key} "
            f"command={command} reason={reason} path={path} ms={ms} detail={detail} targets={targets}\n")


# The rotated generation (oldest first), then the current log.
ROTATED = "".join([
    line(0, "miss", "k1", reason="absent", ms="40.0"),
    line(1, "not-saved", "k1", reason="repository-rule-executed", detail="taint\\x20repository-rule-executed"),
    line(2, "miss", "k1", reason="absent", ms="41.0"),
    line(3, "saved", "k1", ms="900.0", detail="bytes:4096,actions:3,replays:0"),
])
CURRENT = "".join([
    line(4, "hit", "k1", ms="120.0"),
    line(5, "hit", "k1", ms="100.0"),
    line(6, "miss", "k1", reason="read", path="/ws/BUILD.bazel", ms="35.0"),
    line(7, "saved", "k1", ms="800.0"),
    line(8, "hit", "k1", ms="140.0"),
    # Evicted by another key's save: the next `absent` is explained.
    line(9, "evicted", "k1", command="-", reason="count", path="/c/snapshots/v1/k1.snap",
         detail="by:k2,kept:16,bytes:1", targets=""),
    # A key seen only in an eviction (saved before the log began).
    line(9, "evicted", "k0", command="-", reason="count", path="/c/snapshots/v1/k0.snap",
         detail="by:k2,kept:16,bytes:1", targets=""),
    line(10, "miss", "k1", reason="absent", ms="30.0"),
    line(11, "saved", "k1", ms="700.0"),
    # Lost: absent after a save without a removal.
    line(12, "miss", "k1", reason="absent", ms="30.0"),
    # A key that always misses on one tree directory in the cache (a tool
    # writing into it): the saves between do not end the run (L2-W5i).
    line(20, "miss", "k2", command="test", reason="integrity", path="<CACHE>/repositories/trees/ab/sub", ms="50.0",
         targets="//t:t"),
    line(21, "saved", "k2", command="test", ms="600.0", targets="//t:t"),
    line(22, "miss", "k2", command="test", reason="integrity", path="<CACHE>/repositories/trees/ab/sub", ms="51.0",
         targets="//t:t"),
    line(23, "saved", "k2", command="test", ms="600.0", targets="//t:t"),
    line(24, "miss", "k2", command="test", reason="integrity", path="<CACHE>/repositories/trees/ab/sub", ms="52.0",
         detail="INTEGRITY\\x20x", targets="//t:t"),
    line(25, "hit", "k2", command="test", ms="200.0", targets="//t:t"),
    line(26, "healed", "k2", command="test", reason="tree:mode", path="/c/repositories/direct/trees/cd/tool.sh",
         ms="150.0", detail="repository\\x20tree", targets="//t:t"),
    # Verify mode: verified hits by command, one mismatch.
    line(30, "hit", "k3", mode="verify", command="run", ms="300.0", targets="//r:r"),
    line(31, "verified", "k3", mode="verify", command="run", ms="7000.0", targets="//r:r"),
    line(32, "hit", "k3", mode="verify", command="run", ms="310.0", targets="//r:r"),
    line(33, "mismatch", "k3", mode="verify", command="run", reason="taint", path="/c/x", targets="//r:r"),
    line(34, "recorded", "-", mode="record", command="query", reason="no", targets="deps(//...)"),
    "not a log line\n",
])
VERIFY = ("2026-10-24T00:00:33Z 1792800033 key=k3 command=run first=taint differences=2 "
          "taint=repository-rule-executed path=/c/x targets=//r:r\n")
# L2-W5i: the block of that mismatch, and one whose verify.log line rotated away.
OLDER = ("2026-10-23T00:00:00Z 1792713600 key=k9 command=build first=plan differences=1 "
         "taint=- path=- targets=//a:b\n")
DETAILS = (OLDER + "  snapshot: removed (it could not be set aside: x)\n  verify mismatch: plan\n"
           + VERIFY + "  snapshot: set aside as /c/snapshots/mismatched/2026-10-24T000033.0Z-k3-1.snap\n"
           "  verify mismatch: the full analysis would not be saved\n"
           "  verify   taint repository-rule-executed (1): +gen_rule+genrepo\n"
           "    @@ indented detail line @@\n")


class SoakReportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.cache = Path(self.temp.name)
        snapshots = self.cache / "snapshots"
        (snapshots / "v1").mkdir(parents=True)
        (snapshots / "events.log.1").write_text(ROTATED)
        (snapshots / "events.log").write_text(CURRENT.replace("<CACHE>", str(self.cache)))
        (snapshots / "verify.log").write_text(VERIFY)
        (snapshots / "verify-details.log").write_text(DETAILS)
        (snapshots / "mismatched").mkdir()
        (snapshots / "mismatched" / "2026-10-24T000033.0Z-k3-1.snap").write_bytes(b"x" * 5)
        (snapshots / "v1" / "k1.snap").write_bytes(b"x" * 1000)
        (snapshots / "v1" / "k2.snap").write_bytes(b"x" * 24)
        (snapshots / "v1" / ".tmp-k3-1-0").write_bytes(b"x" * 7)
        events, verify_lines, store, details = report.load(self.cache)
        self.report = report.analyze(events, verify_lines, store, details, report.Places(self.cache))

    def tearDown(self):
        self.temp.cleanup()

    def key(self, name):
        return next(info for info in self.report["keys"] if info["key"] == name)

    def test_lines_parse_with_escapes_and_targets(self):
        entry = report.parse_line(
            "2026-10-24T00:00:00Z 1792800000 kind=miss mode=on key=k command=build reason=read "
            "path=/ws/a\\x20b ms=1.5 detail=READ\\x20x targets=//a:b //x\\x20y:\\xc3\\xa9\n")
        self.assertEqual(entry["kind"], "miss")
        self.assertEqual(report.unescape(entry["path"]), "/ws/a b")
        self.assertEqual(entry["targets"], ["//a:b", "//x y:\u00e9"])
        self.assertIsNone(report.parse_line("not a log line"))
        self.assertEqual(report.unescape("a\\x5cb"), "a\\b")

    def test_counts_per_mode_and_kind_read_both_generations(self):
        counts = self.report["counts"]
        self.assertEqual(counts["on/hit"], 4)
        self.assertEqual(counts["on/miss"], 8)
        self.assertEqual(counts["on/saved"], 5)
        self.assertEqual(counts["verify/hit"], 2)
        self.assertEqual(counts["verify/verified"], 1)
        self.assertEqual(counts["verify/mismatch"], 1)
        self.assertEqual(counts["record/recorded"], 1)
        self.assertEqual(self.report["events"], 26)
        self.assertEqual(self.report["misses"]["absent"], 4)
        self.assertEqual(self.report["not_saved"], {"repository-rule-executed": 1})

    def test_hit_rate_on_unchanged_reruns_per_key(self):
        k1 = self.key("k1")
        # Before the first save: two cold misses. After it: 3 hits, a changed
        # input, an absent miss explained by the eviction, and one lost.
        self.assertEqual((k1["before_save"], k1["lookups"], k1["hits"], k1["changed"],
                          k1["after_removal"], k1["unexplained"]), (2, 6, 3, 1, 1, 1))
        self.assertAlmostEqual(k1["hit_rate"], 3 / 4)
        self.assertEqual(k1["command"], "build")
        self.assertEqual(k1["not_saved"], {"repository-rule-executed": 1})
        self.assertEqual(len(self.report["keys"]), 3)
        self.assertEqual(self.report["evicted_only_keys"], 1)

    def test_always_miss_patterns_need_three_misses_without_a_hit(self):
        runs = self.report["always_miss"]
        self.assertEqual(len(runs), 1, runs)
        self.assertEqual((runs[0]["key"], runs[0]["reason"], runs[0]["path"], runs[0]["count"], runs[0]["saves"],
                          runs[0]["place"], runs[0]["class"], runs[0]["detail"]),
                         ("k2", "integrity", f"{self.cache}/repositories/trees/ab/sub", 3, 2, "cache", "suspect",
                          "INTEGRITY x"))
        self.assertEqual(runs[0]["targets"], ["//t:t"])

    def test_heals_verified_hits_mismatches_latency_and_store(self):
        self.assertEqual(self.report["heals"], {"tree:mode": 1})
        self.assertEqual(self.report["heal_lines"][0]["path"], "/c/repositories/direct/trees/cd/tool.sh")
        self.assertEqual(self.report["verified_by_command"], {"run": 1})
        self.assertEqual(self.report["verify_mismatches"], [VERIFY.rstrip("\n")])
        latency = self.report["hit_latency_ms"]
        self.assertEqual(latency["all/all"], {"hits": 6, "p50": 140.0, "p95": 310.0})
        self.assertEqual(latency["on/build"], {"hits": 3, "p50": 120.0, "p95": 140.0})
        self.assertEqual(latency["verify/run"]["hits"], 2)
        store = self.report["store"]
        self.assertEqual((store["snapshots"], store["bytes"]), (2, 1024))
        self.assertEqual(sorted(store["logs"]), ["events.log", "events.log.1", "verify-details.log", "verify.log"])
        self.assertEqual(store["mismatched"], {"snapshots": ["2026-10-24T000033.0Z-k3-1.snap"], "bytes": 5})

    def test_verify_mismatches_pair_with_their_details_blocks(self):
        details = self.report["verify_details"]
        self.assertEqual(len(details), 2, details)
        self.assertEqual(details[0]["line"], VERIFY.rstrip("\n"))
        self.assertEqual(details[0]["lines"], [
            "snapshot: set aside as /c/snapshots/mismatched/2026-10-24T000033.0Z-k3-1.snap",
            "verify mismatch: the full analysis would not be saved",
            "verify   taint repository-rule-executed (1): +gen_rule+genrepo",
            "  @@ indented detail line @@"])
        # A block whose verify.log line rotated away is still shown.
        self.assertEqual((details[1]["line"], details[1].get("only_details")), (OLDER.rstrip("\n"), True))
        # A verify.log line without a block (an older binary).
        paired = report.pair_details(["line without a block"], [])
        self.assertEqual(paired, [{"line": "line without a block", "lines": None}])

    def test_text_and_json_output(self):
        out = io.StringIO()
        with redirect_stdout(out):
            self.assertEqual(report.main([str(self.cache)]), 0)
        text = out.getvalue()
        for expected in ("on/hit", "keys: 3 (and 1 seen only when garbage collection evicted them)",
                         "hit rate on unchanged re-runs 75.0% (3/4)",
                         "always-miss patterns",
                         "heals by reason: tree:mode 1", "verified hits by command: run 1",
                         VERIFY.rstrip("\n"), "p50 140.0, p95 310.0", "store: 2 snapshot(s), 1024 bytes",
                         "SUSPECT 1, likely your edits 0",
                         f"3 x integrity {self.cache}/repositories/trees/ab/sub [cache], 2 save(s) between",
                         "hit rate on unchanged re-runs, all keys: 85.7% (6/7)",
                         "keys never saved (no save, hit or snapshot read in the log): 0",
                         "    snapshot: set aside as /c/snapshots/mismatched/2026-10-24T000033.0Z-k3-1.snap",
                         "      @@ indented detail line @@", "(verify.log line rotated away)",
                         "set aside by verify mismatches (snapshots/mismatched/, the 3 newest kept): 1 snapshot(s)"):
            self.assertIn(expected, text)
        out = io.StringIO()
        with redirect_stdout(out):
            report.main(["--json", str(self.cache)])
        self.assertEqual(json.loads(out.getvalue())["counts"]["on/hit"], 4)
        with tempfile.TemporaryDirectory() as empty, redirect_stdout(io.StringIO()):
            self.assertEqual(report.main([empty]), 2)


def events_of(*specs, key="k", command="build", targets="//a:b"):
    """Parsed events of one key from `(kind, reason, path)` specs, a second apart."""
    return [report.parse_line(line(seconds, kind, key, command=command, reason=reason, path=path,
                                   detail=detail, targets=targets))
            for seconds, (kind, reason, path, detail) in enumerate(
                (spec + ("-",) * (4 - len(spec))) for spec in specs)]


class AlwaysMissTests(unittest.TestCase):
    """L2-W5i (the W5h review's F2): a save ends a run of misses that name an
    input you may have changed; the patterns are SUSPECT (the cache, an output
    tree, no path) or likely your edits (a workspace or host path); keys never
    saved are listed with their reasons."""

    CACHE = "/home/u/code/bazel-rs-out"

    def analyze(self, events):
        return report.analyze(events, [], None, (), report.Places(self.CACHE))

    def test_editing_one_build_file_three_times_is_no_pattern(self):
        saved = ("saved",)
        edit = ("miss", "read", "/home/u/code/pkg/BUILD.bazel", "READ\\x20/home/u/code/pkg/BUILD.bazel:\\x20changed")
        result = self.analyze(events_of(("miss", "absent"), saved, edit, saved, edit, saved, edit, saved, ("hit",)))
        self.assertEqual(result["always_miss"], [])
        info = result["keys"][0]
        self.assertEqual((info["lookups"], info["changed"], info["hits"], info["unexplained"]), (4, 3, 1, 0))
        self.assertAlmostEqual(result["hit_rate_unchanged"]["rate"], 1.0)
        # The same edits while the command kept failing (no save between):
        # likely your edits.
        failed = ("not-saved", "failed")
        result = self.analyze(events_of(("miss", "absent"), saved, edit, failed, edit, failed, edit, failed))
        runs = result["always_miss"]
        self.assertEqual([(run["class"], run["place"], run["count"], run["saves"]) for run in runs],
                         [("likely-your-edits", "elsewhere", 3, 0)])
        self.assertEqual(runs[0]["detail"], "READ /home/u/code/pkg/BUILD.bazel: changed")

    def test_cache_and_output_tree_paths_are_suspect_across_saves(self):
        saved = ("saved",)
        tree = ("miss", "integrity", f"{self.CACHE}/repositories/direct/trees/ab/sub")
        out = ("miss", "readdir", "/home/u/code/bazel-out/k8-fastbuild/bin")
        bazel = ("miss", "stat", "/home/u/.cache/bazel/_bazel_u/1234/execroot/_main")
        for miss in (tree, out, bazel):
            result = self.analyze(events_of(("miss", "absent"), saved, miss, saved, miss, saved, miss, saved))
            runs = result["always_miss"]
            self.assertEqual([(run["class"], run["count"], run["saves"]) for run in runs], [("suspect", 3, 3)],
                             miss)
            self.assertEqual(runs[0]["place"], "cache" if miss is tree else "output")
        # A link named bazel-<workspace> in the workspace (genuine Bazel's
        # execroot) is an output tree too; elsewhere `bazel-` is a name.
        places = report.Places(self.CACHE)
        self.assertEqual(places.of("/home/u/code/bazel-code/external/x"), "output")
        self.assertEqual(places.of("/home/u/code/tools/bazel-helpers/BUILD"), "elsewhere")
        self.assertEqual(places.of("-"), "none")
        self.assertEqual(report.Places("/c", workspace="/w").of("/w/bazel-w/x"), "output")
        self.assertEqual(report.Places("/c").of("/c/repositories/x"), "cache")

    def test_a_snapshot_lost_after_every_save_is_suspect(self):
        saved = ("saved",)
        absent = ("miss", "absent")
        result = self.analyze(events_of(absent, saved, absent, saved, absent, saved, absent))
        runs = result["always_miss"]
        self.assertEqual([(run["class"], run["place"], run["count"], run["saves"]) for run in runs],
                         [("suspect", "none", 3, 2)])
        self.assertEqual(result["keys"][0]["unexplained"], 3)
        # Explained absent misses (after an eviction, while none is saved)
        # are no pattern.
        evicted = ("evicted", "count")
        result = self.analyze(events_of(absent, saved, evicted, absent, saved, evicted, absent, saved, evicted,
                                        absent, ("not-saved", "network"), absent))
        self.assertEqual(result["always_miss"], [])
        info = result["keys"][0]
        self.assertEqual((info["after_removal"], info["unexplained"], info["before_save"]), (3, 0, 2))

    def test_inputs_without_a_path_end_on_a_save_but_are_suspect_without_one(self):
        env = ("miss", "env", "-", "ENV\\x20PATH:\\x20value\\x20changed")
        saved = ("saved",)
        result = self.analyze(events_of(("miss", "absent"), saved, env, saved, env, saved, env, saved))
        self.assertEqual(result["always_miss"], [])
        tainted = ("not-saved", "repository-rule-executed")
        result = self.analyze(events_of(("miss", "absent"), saved, env, tainted, env, tainted, env, tainted))
        self.assertEqual([(run["class"], run["place"], run["count"]) for run in result["always_miss"]],
                         [("suspect", "none", 3)])

    def test_keys_never_saved_are_listed_with_their_reasons(self):
        absent = ("miss", "absent")
        network = ("not-saved", "network")
        events = (events_of(absent, network, absent, network, absent, network, key="n1")
                  + events_of(absent, ("not-saved", "capture"), absent, ("not-saved", "failed"), key="n2")
                  + events_of(absent, key="n3")
                  # A snapshot read by a lookup (saved before the log began).
                  + events_of(("miss", "read", "/home/u/code/BUILD"), key="n4"))
        result = self.analyze(events)
        self.assertEqual([(info["key"], info["lookups"], info["not_saved"], info["expected"])
                          for info in result["never_saved"]],
                         [("n1", 3, {"network": 3}, True), ("n2", 2, {"capture": 1, "failed": 1}, False),
                          ("n3", 1, {}, False)])
        self.assertEqual(result["always_miss"], [])
        text = report.render(result, self.CACHE)
        self.assertIn("keys never saved (no save, hit or snapshot read in the log): 3", text)
        self.assertIn("n1 build //a:b: lookups 3, not saved: network 3 [expected]", text)
        self.assertIn("n2 build //a:b: lookups 2, not saved: capture 1, failed 1 [please report]", text)
        self.assertIn("n3 build //a:b: lookups 1, not saved: no not-saved event [please report]", text)


if __name__ == "__main__":
    unittest.main()
