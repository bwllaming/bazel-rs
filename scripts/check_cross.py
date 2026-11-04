#!/usr/bin/env python3
"""`cargo check` of the static release targets (rust-toolchain.toml's musl
targets) with release.py's zig C toolchain, so code that only fails to compile
for musl (libc types differ from glibc's) is caught before a release tag."""
import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent


def main() -> int:
    zig = release.find_zig(None, ROOT, dict(os.environ))
    if not zig:
        print("check_cross: no zig (run scripts/setup-zig.sh)", file=sys.stderr)
        return 2
    targets = [t for t in release.TARGETS if t.zig]
    with tempfile.TemporaryDirectory() as scratch:
        for target in targets:
            env = dict(os.environ)
            env.update(release.cross_env(target, zig, Path(scratch) / "tools", Path(scratch) / "zig-cache"))
            argv = ["cargo", "check", "--locked", "--target", target.triple, "--lib", "--bins"]
            print("+", " ".join(argv), flush=True)
            if subprocess.run(argv, cwd=ROOT, env=env).returncode != 0:
                return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
