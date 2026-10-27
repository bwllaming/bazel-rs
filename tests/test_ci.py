"""Consistency of the CI workflow's inputs (.github/workflows/ci.yml)."""

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]
LIST = ROOT / "scripts" / "ci-integration-tests.txt"


class CiTest(unittest.TestCase):
    def test_integration_list_names_existing_test_programs(self):
        names = LIST.read_text().split()
        self.assertTrue(names)
        self.assertEqual(names, sorted(set(names)), "sorted, no duplicates")
        for name in names:
            self.assertTrue((ROOT / "tests" / f"{name}.rs").is_file(), name)

    def test_workflow_runs_the_gate_commands(self):
        workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text()
        for needle in (
            "cargo fmt --all -- --check",
            "rustfmt --edition 2024 --check tests/support/*.rs",
            "cargo clippy --locked --all-targets -- -D warnings",
            "cargo test --locked --lib",
            "scripts/ci-integration-tests.txt",
            "rustup toolchain install",
        ):
            self.assertIn(needle, workflow)


if __name__ == "__main__":
    unittest.main()
