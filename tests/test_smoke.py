"""Offline tests of scripts/smoke.py (no binary is run)."""

import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


smoke = load("smoke")
release = sys.modules["release"]


class HostExpectationTest(unittest.TestCase):
    def test_platform_values_per_host(self):
        # @platforms//host: the platforms module's os/cpu values.
        self.assertEqual(smoke.host_expectation("Linux", "x86_64"), ("x86_64", "linux"))
        self.assertEqual(smoke.host_expectation("Linux", "aarch64"), ("aarch64", "linux"))
        self.assertEqual(smoke.host_expectation("Darwin", "arm64"), ("aarch64", "osx"))
        self.assertEqual(smoke.host_expectation("Darwin", "x86_64"), ("x86_64", "osx"))
        with self.assertRaises(release.ReleaseError):
            smoke.host_expectation("Windows", "x86_64")
        with self.assertRaises(release.ReleaseError):
            smoke.host_expectation("Linux", "riscv64")


class NetworkExpectationTest(unittest.TestCase):
    def test_network_smoke_compares_target_cpu_and_os(self):
        calls = []

        def fake_run(argv, cwd=None, env=None, echo=False):
            calls.append(argv)
            out = Path(cwd) / "host.txt"
            out.write_text("aarch64 @platforms//cpu:aarch64,@platforms//os:osx\n")
            return release.Result(0, f"//:host\t{out}\n", "")

        with tempfile.TemporaryDirectory() as scratch:
            release.network_smoke_test(fake_run, Path("/bin/bazel-rs"), Path(scratch), (), "aarch64", os_value="osx")
            with self.assertRaises(release.ReleaseError):
                release.network_smoke_test(fake_run, Path("/bin/bazel-rs"), Path(scratch), (), "aarch64")
        self.assertEqual(len(calls), 2)


if __name__ == "__main__":
    unittest.main()
