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

    def test_workflow_self_builds_with_the_latest_release(self):
        # docs/SELF_BUILD.md: bazel-rs only; the genuine comparison stays local.
        workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text()
        for needle in (
            "self-build:",
            "gh release download",
            "x86_64-unknown-linux-musl",
            'sha256sum -c "$stem.tar.gz.sha256"',
            "python3 scripts/self_build.py check --bazel bazel-rs",
            ".cache/bazel-rs/self-build",
        ):
            self.assertIn(needle, workflow)
        self.assertNotIn("--genuine", workflow)


if __name__ == "__main__":
    unittest.main()
