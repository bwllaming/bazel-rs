"""The Bazel self-build's inputs (scripts/self_build.py, docs/SELF_BUILD.md)."""

from pathlib import Path
import importlib.util
import re
import sys
import tempfile
import tomllib
import unittest
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("self_build", ROOT / "scripts" / "self_build.py")
self_build = importlib.util.module_from_spec(spec)
sys.modules["self_build"] = self_build
spec.loader.exec_module(self_build)


def build_test_names() -> list[str]:
    """The tests/*.rs programs BUILD.bazel lists (quoted names in its test lists)."""
    text = (ROOT / "BUILD.bazel").read_text()
    names = []
    for block in re.findall(r"for name(?:, data)? in \[(.*?)\n    \]", text, re.S):
        names += re.findall(r'^\s+\(?"([a-z_0-9]+)"', block, re.M)
    return names


class SelfBuildTest(unittest.TestCase):
    def test_bazelignore_is_current(self):
        want = self_build.bazelignore_text(self_build.tracked_files(ROOT))
        self.assertEqual((ROOT / ".bazelignore").read_text(), want,
                         "run python3 scripts/self_build.py bazelignore")

    def test_bazelignore_keeps_the_embedded_test_data(self):
        # BUILD.bazel's UNIT_TEST_DATA lives outside every ignored directory.
        entries = self_build.bazelignore_entries(self_build.tracked_files(ROOT))
        build = (ROOT / "BUILD.bazel").read_text()
        data = re.findall(r'"(tests/fixtures/[^"]+)"', build)
        self.assertTrue(data)
        for path in data:
            self.assertTrue((ROOT / path).is_file(), path)
            self.assertFalse(any(path.startswith(e + "/") for e in entries), path)

    def test_bazelignore_entries_cut_fixture_workspaces(self):
        files = [
            "tests/fixtures/a/BUILD",
            "tests/fixtures/a/sub/BUILD.bazel",
            "tests/fixtures/b/workspace/tools/python/BUILD",
            "tests/fixtures/b/reference_receipt.json",
            "tests/fixtures/c/MODULE.bazel",
            "src/BUILD",
        ]
        self.assertEqual(
            self_build.bazelignore_entries(files)[len(self_build.BAZELIGNORE_FIXED):],
            ["tests/fixtures/a", "tests/fixtures/b/workspace"],
        )

    def test_no_tracked_lockfile_holds_an_absolute_path(self):
        # docs/SELF_BUILD.md "The lockfile": crate_universe records the
        # checkout's absolute vendor/ path; a committed copy breaks every other
        # checkout (genuine 9.3.1 and bazel-rs alike).
        for name in self_build.tracked_files(ROOT):
            if name.rsplit("/", 1)[-1] != "MODULE.bazel.lock" or name.startswith("tests/"):
                continue
            text = (ROOT / name).read_text()
            self.assertEqual(self_build.absolute_paths(text), [], name)
        self.assertNotIn("MODULE.bazel.lock", self_build.tracked_files(ROOT))

    def test_absolute_path_finder(self):
        self.assertEqual(
            self_build.absolute_paths('{"path": "/tmp/x/vendor/a", "u": "https://h/x", "l": "//:a"}'),
            ["/tmp/x/vendor/a"],
        )
        self.assertEqual(self_build.absolute_paths('{"a": "@@//pkg:x", "b": "rel/path"}'), [])

    def test_every_embedded_file_is_compile_data(self):
        # include_bytes!/include_str! in src/ must name a file the Bazel
        # build passes to rustc (compile_data or UNIT_TEST_DATA in
        # BUILD.bazel), else the self-build fails where Cargo succeeds.
        import fnmatch
        import re

        build = (ROOT / "BUILD.bazel").read_text()
        strings = re.findall(r'"([^"]*)"', build)
        literals = {s for s in strings if "*" not in s}
        globs = [s.replace("**", "*") for s in strings if "*" in s]
        missing = []
        for source in sorted((ROOT / "src").rglob("*.rs")):
            text = source.read_text()
            for name in re.findall(r'include_(?:bytes|str)!\(\s*"([^"{}]+)"\s*\)', text):
                path = (source.parent / name).resolve().relative_to(ROOT).as_posix()
                if path in literals or any(fnmatch.fnmatch(path, g) for g in globs):
                    continue
                missing.append(f"{source.relative_to(ROOT)}: {path}")
        self.assertEqual(missing, [], "add these to compile_data in BUILD.bazel")

    def test_rust_toolchain_matches_rust_toolchain_toml(self):
        channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        module = (ROOT / "MODULE.bazel").read_text()
        self.assertIn(f'versions = ["{channel}"]', module)

    def test_bazel_integration_tests_are_hermetic_ci_programs(self):
        names = build_test_names()
        self.assertGreaterEqual(len(names), 10)
        ci = set((ROOT / "scripts" / "ci-integration-tests.txt").read_text().split())
        for name in names:
            self.assertTrue((ROOT / "tests" / f"{name}.rs").is_file(), name)
            self.assertIn(name, ci, f"{name}: not in scripts/ci-integration-tests.txt")

    def test_process_summary_and_built_binary(self):
        run = self_build.Run(0, "", (
            "Target //:bazel-rs up-to-date:\n  bazel-bin/bazel-rs\n"
            "INFO: Elapsed time: 28.5s, Critical Path: 1.0s\n"
            "INFO: 1025 processes: 759 remote cache hit, 266 internal.\n"
        ), 30.0)
        self.assertEqual(run.processes(),
                         {"total": 1025, "remote cache hit": 759, "internal": 266})
        self.assertEqual(run.elapsed(), 28.5)
        self.assertEqual(self_build.built_binary(run.stderr), ROOT / "bazel-bin" / "bazel-rs")
        self.assertEqual(
            self_build.built_binary("Target //:bazel-rs up-to-date:\n  /abs/bazel-rs\n"),
            Path("/abs/bazel-rs"),
        )
        with self.assertRaises(self_build.SelfBuildError):
            self_build.built_binary("")
        self.assertEqual(self_build.Run(0, "", "INFO: 1 process: 1 internal.\n", 1).processes(),
                         {"total": 1, "internal": 1})

    def test_cache_server_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp, self_build.CacheServer(Path(tmp)) as cache:
            url = f"{cache.url}/cas/abc"
            with self.assertRaises(urllib.error.HTTPError) as missing:
                urllib.request.urlopen(url)
            self.assertEqual(missing.exception.code, 404)
            put = urllib.request.Request(url, data=b"blob", method="PUT")
            self.assertEqual(urllib.request.urlopen(put).status, 200)
            self.assertEqual(urllib.request.urlopen(url).read(), b"blob")
            self.assertEqual(cache.take_stats(),
                             {"get_hit_cas": 1, "get_miss_cas": 1, "put_cas": 1})


if __name__ == "__main__":
    unittest.main()
