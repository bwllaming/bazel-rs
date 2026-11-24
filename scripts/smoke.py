#!/usr/bin/env python3
"""Smoke test of a built bazel-rs binary (CI's macOS job, docs/MACOS.md).

    scripts/smoke.py BINARY [--network]

Runs release.py's offline smoke test against BINARY in a fresh scratch
directory: --version, version, help, a query and two builds of a small
genrule workspace (the explicit-module profile, a dead proxy: nothing is
downloaded). --network adds release.py's built-in profile build, which
materializes the binary's embedded @bazel_tools and resolves @platforms//host
from the Bazel Central Registry, and checks the detected host platform.
Python 3.11+ standard library only.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release  # noqa: E402


def host_expectation(system: str, machine: str) -> tuple[str, str]:
    """(`@platforms//cpu` value, `@platforms//os` value) of a host.

    `system`/`machine` are os.uname()'s sysname and machine.
    """
    cpu = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}.get(machine)
    if cpu is None:
        raise release.ReleaseError(f"unsupported host machine {machine!r}")
    os_value = {"Darwin": "osx", "Linux": "linux"}.get(system)
    if os_value is None:
        raise release.ReleaseError(f"unsupported host system {system!r}")
    return cpu, os_value


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binary", type=Path, help="the bazel-rs binary to test")
    parser.add_argument("--network", action="store_true", help="also the built-in profile build (downloads from the BCR)")
    args = parser.parse_args(argv)
    binary = args.binary.resolve()
    version = release.cargo_toml_version((release.ROOT / "Cargo.toml").read_text())
    uname = os.uname()
    try:
        with tempfile.TemporaryDirectory(prefix="bazel-rs-smoke-") as scratch:
            compat = release.smoke_test(release.run_command, binary, version, Path(scratch))
            if args.network:
                cpu, os_value = host_expectation(uname.sysname, uname.machine)
                release.network_smoke_test(release.run_command, binary, Path(scratch), (), cpu, os_value=os_value)
    except release.ReleaseError as error:
        print(f"smoke: {error}", file=sys.stderr)
        return 1
    print(f"smoke: {binary} ({version}, compatibility target Bazel {compat}) passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
