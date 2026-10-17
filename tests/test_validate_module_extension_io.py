"""Retired module-extension I/O validator (N43b).

scripts/validate_module_extension_io.py validated the removed native
owned-scratch module_ctx I/O model. Its former 2,216-line harness tests
(replay integrity, frozen runtime ledgers pinning src/extension_ctx.rs and
src/extension_io.rs, native collector receipts) described that model only and
were removed with N43b. What still holds, and is tested here:

* the 33 case recipes and the HTTP response plan stay well-formed data;
* every capture/replay mode refuses (only `--plan` runs, launching nothing);
* every recipe was replayed on genuine Bazel 9.3 and native by
  artifacts/ext_ctx-capture.py --targeted-n43b, and the receipt accounts for
  every case (no unexplained mismatch).

No test here compares against pinned hashes of current or former Rust sources.
"""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("module_io_retired", ROOT / "scripts/validate_module_extension_io.py")
Q = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = Q
SPEC.loader.exec_module(Q)
RECEIPT = ROOT / "tests/fixtures/ext_ctx_surface/reference_receipt.json"


class RetiredValidatorTests(unittest.TestCase):
    def test_case_recipes_and_response_plan_are_well_formed(self):
        cases = Q.cases()
        self.assertEqual(len(cases), 33)
        self.assertEqual(len({case.name for case in cases}), 33)
        routes = Q.response_plan()
        for case in cases:
            for op in case.operations:
                self.assertIn(op["method"], ("download", "read"))
                if op["method"] == "download":
                    urls = op["args"][0] if type(op["args"][0]) is list else [op["args"][0]]
                    for url in urls:
                        self.assertTrue(url.startswith("/"), url)
            for path in case.requests:
                self.assertIn(path, routes, case.name)
            self.assertTrue(set(case.flags) <= {"--repository_disable_download"})
        for route in routes.values():
            data = Q.unblob(route["body"])
            self.assertEqual(route["headers"]["Content-Length"], str(len(data)))

    def test_every_capture_and_replay_mode_refuses(self):
        for argv in (["--native", "/bin/true"], ["--native-replay"], ["--paired-replay"],
                     ["--reference-only"], ["--replay-reference"], ["--preparation-replay"],
                     ["--setup-only"], ["--api-smoke-only"], ["--native-preparation-replay"]):
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                self.assertEqual(Q.main(argv), 2, argv)
            self.assertIn("retired (N43b)", stderr.getvalue())

    def test_plan_still_launches_nothing(self):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            self.assertEqual(Q.main(["--plan"]), 0)
        self.assertEqual(len(json.loads(stdout.getvalue())["cases"]), 33)

    def test_every_recipe_has_a_paired_genuine_row(self):
        receipt = json.loads(RECEIPT.read_text())
        section = receipt["targeted_rerun_n43b"]
        rows = {row["case"]: row for row in section["comparisons"]}
        self.assertEqual(set(rows), {case.name for case in Q.cases()})
        self.assertEqual(section["validator_sha256"] and len(section["validator_sha256"]), 64)
        explained = " ".join(change["change"] for change in receipt["post_capture_changes"])
        for name, row in rows.items():
            if row["verdict"] == "MISMATCH":
                # Only fixed-after-capture rows, each named in post_capture_changes.
                self.assertIn(name, explained)
            else:
                self.assertIn(row["verdict"], ("MATCH", "EXACT_MESSAGE"))
                if row["verdict"] == "EXACT_MESSAGE":
                    self.assertIn(row["reference_message"].split(" ")[0], row["native_message"])


if __name__ == "__main__":
    unittest.main()
