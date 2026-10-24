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
    # A key that always misses on one tree directory (a tool writing into it).
    line(20, "miss", "k2", command="test", reason="integrity", path="/c/repositories/trees/ab/sub", ms="50.0",
         targets="//t:t"),
    line(21, "saved", "k2", command="test", ms="600.0", targets="//t:t"),
    line(22, "miss", "k2", command="test", reason="integrity", path="/c/repositories/trees/ab/sub", ms="51.0",
         targets="//t:t"),
    line(23, "saved", "k2", command="test", ms="600.0", targets="//t:t"),
    line(24, "miss", "k2", command="test", reason="integrity", path="/c/repositories/trees/ab/sub", ms="52.0",
         targets="//t:t"),
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
        (snapshots / "events.log").write_text(CURRENT)
        (snapshots / "verify.log").write_text(VERIFY)
        (snapshots / "verify-details.log").write_text(DETAILS)
        (snapshots / "mismatched").mkdir()
        (snapshots / "mismatched" / "2026-10-24T000033.0Z-k3-1.snap").write_bytes(b"x" * 5)
        (snapshots / "v1" / "k1.snap").write_bytes(b"x" * 1000)
        (snapshots / "v1" / "k2.snap").write_bytes(b"x" * 24)
        (snapshots / "v1" / ".tmp-k3-1-0").write_bytes(b"x" * 7)
        events, verify_lines, store, details = report.load(self.cache)
        self.report = report.analyze(events, verify_lines, store, details)

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
        self.assertEqual((runs[0]["key"], runs[0]["reason"], runs[0]["path"], runs[0]["count"]),
                         ("k2", "integrity", "/c/repositories/trees/ab/sub", 3))
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
                         "always-miss patterns", "3 x integrity /c/repositories/trees/ab/sub",
                         "heals by reason: tree:mode 1", "verified hits by command: run 1",
                         VERIFY.rstrip("\n"), "p50 140.0, p95 310.0", "store: 2 snapshot(s), 1024 bytes",
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


if __name__ == "__main__":
    unittest.main()
