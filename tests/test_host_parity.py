"""scripts/host_parity.py: normalization and the known-difference filter
(docs/MACOS.md, "Genuine parity on macOS")."""

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))

import host_parity  # noqa: E402


class HostParityTest(unittest.TestCase):
    def test_normalizer_maps_both_tools_spawn_roots_and_dirs(self):
        norm = host_parity.Normalizer([("/ob/x", "<OUTPUT_BASE>"), ("/state/cache", "<STATE>")])
        self.assertEqual(
            norm("/ob/x/sandbox/darwin-sandbox/12/execroot/_main/bazel-out"),
            "<SPAWN_ROOT>/bazel-out",
        )
        self.assertEqual(
            norm("/state/cache/work/0a1b-123-4/execroot/bazel-out"),
            "<SPAWN_ROOT>/bazel-out",
        )
        self.assertEqual(norm('(literal "/ob/x/sandbox/darwin-sandbox/3/stats.out")'),
                         '(literal "<SPAWN_DIR>/stats.out")')
        self.assertEqual(norm('(literal "/state/cache/work/ff-1-2/stats.out")'),
                         '(literal "<SPAWN_DIR>/stats.out")')
        self.assertEqual(norm("/state/cache/actions/" + "a" * 64 + "/outputs/bazel-out/x"),
                         "<EXECROOT>/bazel-out/x")
        self.assertEqual(norm('<testcase time="1.25" name="t">'), '<testcase time="T" name="t">')
        self.assertEqual(norm("# Configuration: " + "0" * 64), "# Configuration: <C>")

    def test_cquery_lines_drop_checksums_and_canonical_spelling(self):
        text = "@@rules_shell+//shell:x (abcdef1)\n@rules_shell//shell:x (1234567)\n//:a.txt (null)\n"
        self.assertEqual(host_parity.cquery_lines(text), "//:a.txt (null)\n@rules_shell//shell:x (C)\n")

    def test_processes_line_keeps_the_strategies(self):
        self.assertEqual(
            host_parity.processes_line("INFO: 7 processes: 2 action cache hit, 4 internal, 3 darwin-sandbox."),
            "INFO: processes: 3 darwin-sandbox",
        )

    def test_aquery_blocks_sorted(self):
        text = "action 'b'\n  x\n\naction 'a'\n  y\n\n"
        self.assertEqual(host_parity.aquery_blocks(text), "action 'a'\n  y\n\naction 'b'\n  x\n\n")

    def test_sorted_blocks_sort_indented_runs_only(self):
        self.assertEqual(host_parity.sorted_blocks("(a\n  z\n  y\n)\n(b\n"), "(a\n  y\n  z\n)\n(b\n")

    def test_known_reason_needs_every_diff_line_known(self):
        diff = "--- genuine\n+++ bazel-rs\n@@ -1 +1 @@\n-source_input_symlink=yes\n+source_input_symlink=no\n"
        args = ["build", "//..."]
        self.assertIsNotNone(host_parity.known_reason("darwin_sandbox", args, "bazel-bin/default.txt", diff))
        self.assertIsNone(host_parity.known_reason("darwin_sandbox", args, "bazel-bin/default.txt",
                                                   diff + "-write_home=denied\n+write_home=allowed\n"))
        self.assertIsNone(host_parity.known_reason("host_parity", args, "bazel-bin/default.txt", diff))

    def test_cases_name_existing_fixtures(self):
        for case in host_parity.CASES:
            self.assertTrue((host_parity.FIXTURES / case["workspace"] / "MODULE.bazel").is_file(), case["name"])

    def test_genuine_pins(self):
        self.assertEqual((ROOT / ".bazelversion").read_text().strip(), "9.3.1")
        self.assertIn("bazel-9.3.1-darwin-arm64", host_parity.GENUINE_SHA256)


if __name__ == "__main__":
    unittest.main()
