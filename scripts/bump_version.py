#!/usr/bin/env python3
"""Set the bazel-rs version for the next release (docs/RELEASING.md).

    scripts/bump_version.py X.Y.Z [--dry-run]

Rewrites Cargo.toml's [package] version, Cargo.lock's root `bazel-rs` entry and
the test files that assert the native `bazel-rs --version` text literally. Every
other version string derives from CARGO_PKG_VERSION. Captured receipts under
tests/fixtures (recorded outputs of past binaries) and docs examples are left
alone; the remaining mentions of the old version are listed for review.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import subprocess
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from release import ReleaseError, cargo_lock_version, cargo_toml_version, parse_version  # noqa: E402


def _package_version(text: str, old: str, new: str) -> str:
    # The first `version = "..."` line of the [package] table.
    match = re.search(r'(?ms)^\[package\]\n(?:(?!^\[).)*?^version = "([^"]*)"$', text)
    if not match or match.group(1) != old:
        raise ReleaseError(f"Cargo.toml: [package] version {old!r} not found")
    start, end = match.span(1)
    return text[:start] + new + text[end:]


def _lock_root(text: str, old: str, new: str) -> str:
    pattern = re.compile(r'(?m)^\[\[package\]\]\nname = "bazel-rs"\nversion = "([^"]*)"\n(?!source = )')
    matches = list(pattern.finditer(text))
    if len(matches) != 1 or matches[0].group(1) != old:
        raise ReleaseError(f"Cargo.lock: root bazel-rs {old!r} entry not found exactly once")
    start, end = matches[0].span(1)
    return text[:start] + new + text[end:]


def _literal(needle_template: str) -> callable:
    def edit(text: str, old: str, new: str) -> str:
        needle = needle_template.format(version=old)
        if text.count(needle) != 1:
            raise ReleaseError(f"expected exactly one {needle!r}")
        return text.replace(needle, needle_template.format(version=new))

    return edit


VERSION_TEXT = "bazel-rs {version} (experimental; compatibility target Bazel 9.3.1)"
EDITS = {
    "Cargo.toml": _package_version,
    "Cargo.lock": _lock_root,
    # Asserts the live native binary's `version` output.
    "tests/fixtures/module_extension_io/native_capture.py": _literal(VERSION_TEXT),
    # Synthetic native `version` row of the label-passthrough validator test.
    "tests/test_validate_label_passthrough.py": _literal(VERSION_TEXT),
}


def bump(files: dict[str, str], old: str, new: str) -> dict[str, str]:
    """New contents of every file in EDITS (pure: `files` maps path -> text)."""
    out = {}
    for path, edit in EDITS.items():
        try:
            out[path] = edit(files[path], old, new)
        except ReleaseError as error:
            raise ReleaseError(f"{path}: {error}") from None
    if cargo_toml_version(out["Cargo.toml"]) != new or cargo_lock_version(out["Cargo.lock"]) != new:
        raise ReleaseError("Cargo.toml/Cargo.lock did not end up at the new version")
    return out


def remaining_mentions(root: Path, old: str) -> list[str]:
    pattern = rf"bazel-rs[ /.]{re.escape(old)}\b"
    result = subprocess.run(
        ["git", "-C", str(root), "grep", "-n", "-E", "-I", pattern, "--", ".", ":!vendor"],
        text=True,
        capture_output=True,
    )
    return result.stdout.splitlines()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("version", help="new version X.Y.Z")
    parser.add_argument("--dry-run", action="store_true", help="show what would change")
    args = parser.parse_args(argv)
    try:
        parse_version(args.version)
        files = {path: (ROOT / path).read_text() for path in EDITS}
        old = cargo_toml_version(files["Cargo.toml"])
        if old == args.version:
            raise ReleaseError(f"already at {old}")
        new_files = bump(files, old, args.version)
    except (ReleaseError, tomllib.TOMLDecodeError) as error:
        print(f"bump_version.py: {error}", file=sys.stderr)
        return 2
    for path, text in new_files.items():
        print(f"{'would update' if args.dry_run else 'updated'} {path}: {old} -> {args.version}")
        if not args.dry_run:
            (ROOT / path).write_text(text)
    others = [line for line in remaining_mentions(ROOT, old) if line.split(":", 1)[0] not in EDITS]
    if others:
        print(f"\nleft unchanged (captured receipts/examples of earlier binaries), review:")
        for line in others:
            print(f"  {line[:160]}")
    if not args.dry_run:
        print(f"\nnext: cargo build --locked && commit `release: bump version to {args.version}`")
    return 0


if __name__ == "__main__":
    sys.exit(main())
