"""Offline tests of scripts/release.py and scripts/bump_version.py (pure parts only).

No cargo, network, gh or real git commands: git and gh are fake runners.
"""

import gzip
import hashlib
import importlib.util
import io
from pathlib import Path
import sys
import tarfile
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


release = load("release")
bump_version = load("bump_version")
Result = release.Result

HEAD = "a" * 40
CARGO_TOML = '[package]\nname = "bazel-rs"\nversion = "1.2.3"\nrepository = "https://github.com/o/r"\n'
CARGO_LOCK = (
    'version = 4\n\n[[package]]\nname = "anyhow"\nversion = "1.0.0"\n'
    'source = "registry+https://github.com/rust-lang/crates.io-index"\n\n'
    '[[package]]\nname = "bazel-rs"\nversion = "1.2.3"\ndependencies = [\n "anyhow",\n]\n'
)


class FakeRunner:
    """Answers git/gh commands of a releasable repository; overrides per test."""

    def __init__(self, overrides=None):
        self.calls = []
        self.answers = {
            ("symbolic-ref", "--quiet", "--short", "HEAD"): Result(0, "main\n"),
            ("status", "--porcelain", "--untracked-files=normal"): Result(0, ""),
            ("rev-parse", "--verify", "HEAD^{commit}"): Result(0, HEAD + "\n"),
            ("fetch", "--quiet", "origin"): Result(0),
            ("rev-parse", "--verify", "--quiet", "refs/remotes/origin/main"): Result(0, HEAD + "\n"),
            ("rev-list", "--left-right", "--count", "HEAD...refs/remotes/origin/main"): Result(0, "0\t0\n"),
            ("show", f"{HEAD}:Cargo.toml"): Result(0, CARGO_TOML),
            ("show", f"{HEAD}:Cargo.lock"): Result(0, CARGO_LOCK),
            ("rev-parse", "--quiet", "--verify", "refs/tags/v1.2.3"): Result(1),
            ("ls-remote", "--tags", "origin", "refs/tags/v1.2.3"): Result(0, ""),
            ("gh", "auth", "status", "--hostname", "github.com"): Result(0),
            ("gh", "release", "view", "v1.2.3", "--repo", "o/r", "--json", "tagName"): Result(
                1, "", "release not found\n"
            ),
        }
        self.answers.update(overrides or {})

    def key(self, argv):
        argv = [str(a) for a in argv]
        if argv[:2] == ["git", "-C"]:
            return tuple(argv[3:])
        return ("gh", *argv[1:]) if argv[0].endswith("gh") else tuple(argv)

    def __call__(self, argv, **kwargs):
        key = self.key(argv)
        self.calls.append(key)
        if key not in self.answers:
            raise AssertionError(f"unexpected command {key}")
        return self.answers[key]


def run_preflight(runner, version="1.2.3", gh="/x/gh", allow_dirty=False):
    return release.preflight(runner, version, repo_dir=Path("/repo"), gh=gh, gh_repo="o/r", allow_dirty=allow_dirty)


class VersionTest(unittest.TestCase):
    def test_parse(self):
        self.assertEqual(release.parse_version("0.1.0"), (0, 1, 0, None))
        self.assertEqual(release.parse_version("1.20.3-rc.1"), (1, 20, 3, "rc.1"))
        for bad in ("1.2", "v1.2.3", "01.2.3", "1.2.3.4", "1.2.3-", ""):
            with self.assertRaises(release.ReleaseError, msg=bad):
                release.parse_version(bad)

    def test_names_and_prerelease(self):
        self.assertEqual(release.tag_name("0.1.0"), "v0.1.0")
        self.assertEqual(release.archive_stem("0.1.0"), "bazel-rs-0.1.0-x86_64-unknown-linux-gnu")
        self.assertTrue(release.is_prerelease("0.1.0"))
        self.assertFalse(release.is_prerelease("0.1.0", latest=True))
        self.assertTrue(release.is_prerelease("1.0.0-rc.1"))
        self.assertFalse(release.is_prerelease("1.0.0"))
        keys = sorted(["0.10.0", "0.2.0", "1.0.0", "1.0.0-rc.1"], key=release.version_key)
        self.assertEqual(keys, ["0.2.0", "0.10.0", "1.0.0-rc.1", "1.0.0"])

    def test_cargo_files(self):
        self.assertEqual(release.cargo_toml_version(CARGO_TOML), "1.2.3")
        self.assertEqual(release.cargo_lock_version(CARGO_LOCK), "1.2.3")
        self.assertEqual(release.repository_slug(CARGO_TOML), "o/r")
        with self.assertRaises(release.ReleaseError):
            release.repository_slug('[package]\nrepository = "https://gitlab.com/o/r"\n')

    def test_repository_metadata_is_consistent(self):
        toml = (ROOT / "Cargo.toml").read_text()
        self.assertEqual(release.cargo_toml_version(toml), release.cargo_lock_version((ROOT / "Cargo.lock").read_text()))
        self.assertEqual(release.repository_slug(toml), "bwllaming/bazel-rs")
        self.assertIn('license = "Apache-2.0"', toml)
        for name in release.DOC_FILES:
            self.assertTrue((ROOT / name).is_file(), name)
        self.assertTrue((ROOT / "LICENSE").read_text().lstrip().startswith("Apache License"))

    def test_version_output(self):
        line = "bazel-rs 1.2.3 (experimental; compatibility target Bazel 9.3.0)\n"
        self.assertEqual(release.check_version_output(line, "1.2.3"), "9.3.0")
        with self.assertRaises(release.ReleaseError):
            release.check_version_output(line, "1.2.4")
        with self.assertRaises(release.ReleaseError):
            release.check_version_output("Build label: 9.3.0\n", "1.2.3")


class PreflightTest(unittest.TestCase):
    def test_releasable(self):
        runner = FakeRunner()
        problems, head = run_preflight(runner)
        self.assertEqual(problems, [])
        self.assertEqual(head, HEAD)
        # Read-only apart from fetch: never tags, pushes or creates.
        for call in runner.calls:
            self.assertNotIn("push", call)
            self.assertNotIn("create", call)
            self.assertNotEqual(call[:1], ("tag",))

    def assertProblem(self, runner, needle, **kwargs):
        problems, _ = run_preflight(runner, **kwargs)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn(needle, problems[0])

    def test_branch(self):
        self.assertProblem(
            FakeRunner({("symbolic-ref", "--quiet", "--short", "HEAD"): Result(0, "feature\n")}),
            "not `main`",
        )
        self.assertProblem(FakeRunner({("symbolic-ref", "--quiet", "--short", "HEAD"): Result(1)}), "detached")

    def test_dirty(self):
        dirty = {("status", "--porcelain", "--untracked-files=normal"): Result(0, " M src/main.rs\n?? x\n")}
        self.assertProblem(FakeRunner(dirty), "not clean (2 entries")
        self.assertEqual(run_preflight(FakeRunner(dirty), allow_dirty=True)[0], [])

    def test_not_pushed(self):
        runner = FakeRunner(
            {
                ("rev-parse", "--verify", "--quiet", "refs/remotes/origin/main"): Result(0, "b" * 40 + "\n"),
                ("rev-list", "--left-right", "--count", "HEAD...refs/remotes/origin/main"): Result(0, "3\t0\n"),
            }
        )
        self.assertProblem(runner, "3 ahead, 0 behind): push main yourself")
        self.assertProblem(
            FakeRunner({("rev-parse", "--verify", "--quiet", "refs/remotes/origin/main"): Result(1)}),
            "origin/main does not exist",
        )
        self.assertProblem(FakeRunner({("fetch", "--quiet", "origin"): Result(128, "", "no route")}), "fetch origin` failed")

    def test_version_mismatch(self):
        problems, _ = run_preflight(
            FakeRunner(
                {
                    ("rev-parse", "--quiet", "--verify", "refs/tags/v2.0.0"): Result(1),
                    ("ls-remote", "--tags", "origin", "refs/tags/v2.0.0"): Result(0, ""),
                    ("gh", "release", "view", "v2.0.0", "--repo", "o/r", "--json", "tagName"): Result(1, "", "release not found"),
                }
            ),
            version="2.0.0",
        )
        self.assertEqual(len(problems), 2, problems)
        self.assertIn("Cargo.toml says 1.2.3, releasing 2.0.0", problems[0])
        self.assertIn("Cargo.lock says 1.2.3", problems[1])

    def test_existing_tag_and_release(self):
        self.assertProblem(
            FakeRunner({("rev-parse", "--quiet", "--verify", "refs/tags/v1.2.3"): Result(0, HEAD)}),
            "already exists locally",
        )
        self.assertProblem(
            FakeRunner({("ls-remote", "--tags", "origin", "refs/tags/v1.2.3"): Result(0, f"{HEAD}\trefs/tags/v1.2.3\n")}),
            "already exists on origin",
        )
        self.assertProblem(
            FakeRunner({("gh", "release", "view", "v1.2.3", "--repo", "o/r", "--json", "tagName"): Result(0, "{}")}),
            "GitHub release v1.2.3 already exists",
        )
        self.assertProblem(
            FakeRunner(
                {("gh", "release", "view", "v1.2.3", "--repo", "o/r", "--json", "tagName"): Result(1, "", "HTTP 401")}
            ),
            "cannot check",
        )

    def test_gh(self):
        self.assertProblem(FakeRunner(), "gh (GitHub CLI) not found", gh=None)
        self.assertProblem(
            FakeRunner({("gh", "auth", "status", "--hostname", "github.com"): Result(1, "", "not logged in")}),
            "not authenticated",
        )


class BuildTest(unittest.TestCase):
    def test_build_env(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            (repo / ".direnv" / "rustup").mkdir(parents=True)
            base = {"PATH": "/bin", "RUSTFLAGS": "-Cdebuginfo=2", "CARGO_TARGET_DIR": "/elsewhere", "RUSTUP_TOOLCHAIN": "nightly", "CARGO_HOME": "/ch"}
            env = release.build_env(base, repo, Path("/w/src"), Path("/w/target"), 42)
        self.assertEqual(env["RUSTUP_HOME"], str(repo / ".direnv" / "rustup"))
        self.assertEqual(env["CARGO_HOME"], "/ch")
        self.assertEqual(env["CARGO_TARGET_DIR"], "/w/target")
        self.assertEqual(env["SOURCE_DATE_EPOCH"], "42")
        self.assertNotIn("RUSTUP_TOOLCHAIN", env)
        self.assertEqual(env["RUSTFLAGS"], "--remap-path-prefix=/w/src=/bazel-rs --remap-path-prefix=/ch=/cargo")

    def test_glibc_and_elf(self):
        text = "0 DF *UND* GLIBC_2.2.5 memcpy\n0 DF *UND* GLIBC_2.39 x\n0 DF *UND* GLIBC_2.34 y\nGLIBC_PRIVATE\n"
        self.assertEqual(release.max_glibc(text), "2.39")
        self.assertIsNone(release.max_glibc("no versions"))
        with tempfile.TemporaryDirectory() as tmp:
            elf = Path(tmp) / "bin"
            elf.write_bytes(b"\x7fELF\x02\x01\x01" + bytes(11) + (62).to_bytes(2, "little") + bytes(10))
            self.assertEqual(release.elf_machine(elf), 62)
            elf.write_bytes(b"#!/bin/sh\n" + bytes(20))
            self.assertIsNone(release.elf_machine(elf))


class PackageTest(unittest.TestCase):
    FILES = [
        ("bazel-rs", b"\x7fELF binary", 0o775),
        ("README.md", b"readme\n", 0o664),
        ("LICENSE", b"license\n", 0o600),
    ]

    def test_tarball_is_deterministic(self):
        with tempfile.TemporaryDirectory() as a, tempfile.TemporaryDirectory() as b:
            first = Path(a) / "x.tar.gz"
            second = Path(b) / "x.tar.gz"
            sha = release.deterministic_tarball(first, "root-1", self.FILES, 1_700_000_000)
            # Different input order, same result.
            self.assertEqual(release.deterministic_tarball(second, "root-1", list(reversed(self.FILES)), 1_700_000_000), sha)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            self.assertEqual(sha, hashlib.sha256(first.read_bytes()).hexdigest())
            raw = first.read_bytes()
            self.assertEqual(raw[4:8], b"\0\0\0\0")  # gzip mtime 0
            self.assertEqual(raw[3] & 0x08, 0)  # no file name
            with tarfile.open(first) as tar:
                members = tar.getmembers()
                self.assertEqual(
                    [m.name for m in members],
                    ["root-1", "root-1/LICENSE", "root-1/README.md", "root-1/bazel-rs"],
                )
                for m in members:
                    self.assertEqual((m.uid, m.gid, m.uname, m.gname, m.mtime), (0, 0, "root", "root", 1_700_000_000))
                self.assertEqual([m.mode for m in members], [0o755, 0o644, 0o644, 0o755])
                self.assertEqual(tar.extractfile("root-1/bazel-rs").read(), b"\x7fELF binary")
            third = Path(b) / "y.tar.gz"
            self.assertNotEqual(release.deterministic_tarball(third, "root-1", self.FILES, 1), sha)

    def test_checksums(self):
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            asset = dist / "a.tar.gz"
            asset.write_bytes(b"abc")
            written = release.write_checksums(dist, [asset])
            want = hashlib.sha256(b"abc").hexdigest() + "  a.tar.gz\n"
            self.assertEqual([p.name for p in written], ["a.tar.gz.sha256", "SHA256SUMS"])
            self.assertEqual((dist / "a.tar.gz.sha256").read_text(), want)
            self.assertEqual((dist / "SHA256SUMS").read_text(), want)


class LicensesTest(unittest.TestCase):
    METADATA = {
        "packages": [
            {"id": "root", "name": "bazel-rs", "version": "0.1.0", "source": None},
            {"id": "a", "name": "alpha", "version": "1.0.0", "license": "MIT", "repository": "https://x/a", "source": "registry+r"},
            {"id": "b", "name": "beta", "version": "2.0.0", "license": "Apache-2.0", "repository": None, "source": None},
            {"id": "d", "name": "devonly", "version": "1.0.0", "license": "MIT", "source": "registry+r"},
            {"id": "c", "name": "cc", "version": "1.0.0", "license": "MIT OR Apache-2.0", "source": "registry+r"},
        ],
        "resolve": {
            "root": "root",
            "nodes": [
                {"id": "root", "deps": [
                    {"pkg": "a", "dep_kinds": [{"kind": None}]},
                    {"pkg": "d", "dep_kinds": [{"kind": "dev"}]},
                    {"pkg": "b", "dep_kinds": [{"kind": "dev"}, {"kind": None}]},
                ]},
                {"id": "a", "deps": [{"pkg": "c", "dep_kinds": [{"kind": "build"}]}]},
                {"id": "b", "deps": []},
                {"id": "c", "deps": []},
                {"id": "d", "deps": []},
            ],
        },
    }

    def test_runtime_packages(self):
        names = [p["name"] for p in release.runtime_packages(self.METADATA)]
        self.assertEqual(names, ["alpha", "beta", "cc"])

    def test_license_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            crate = Path(tmp)
            (crate / "LICENSE-MIT").write_text("mit")
            (crate / "src").mkdir()
            (crate / "src" / "lib.rs").write_text("")
            (crate / "jemalloc").mkdir()
            (crate / "jemalloc" / "COPYING").write_text("bsd")
            (crate / "tests").mkdir()
            (crate / "tests" / "LICENSE").write_text("ignored")
            self.assertEqual(release.license_files(crate), [("LICENSE-MIT", b"mit"), ("jemalloc/COPYING", b"bsd")])

    def test_markdown_dedups_texts(self):
        files = {"alpha": [("LICENSE", b"MIT text")], "beta": [], "cc": [("LICENSE-MIT", b"MIT text")]}
        text = release.third_party_markdown(
            "0.1.0", HEAD, release.runtime_packages(self.METADATA), lambda p: files[p["name"]],
            b"bazel license ```", [("jdk/legal/x/LICENSE", b"gpl"), ("jdk/legal/y/LICENSE", b"gpl")],
        )
        self.assertIn("| alpha | 1.0.0 | MIT | https://x/a |", text)
        self.assertIn("| beta (vendored) | 2.0.0 | Apache-2.0 | - |", text)
        self.assertEqual(text.count("MIT text"), 1)
        self.assertIn("`cc 1.0.0/LICENSE-MIT`: same text as `alpha 1.0.0/LICENSE`.", text)
        self.assertIn("`embedded_tools/jdk/legal/y/LICENSE`: same text as `embedded_tools/jdk/legal/x/LICENSE`.", text)
        self.assertIn("````text\nbazel license ```\n````", text)
        self.assertIn("No license file in the crate; license expression: Apache-2.0.", text)

    def test_bundle_legal_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / "t.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                for name, data in (("jdk/legal/java.base/aes.md", b"aes"), ("jdk/bin/java", b"elf"), ("jdk/ASSEMBLY_EXCEPTION", b"ae"), ("tools/LICENSE.txt", b"l")):
                    info = tarfile.TarInfo(name)
                    info.size = len(data)
                    tar.addfile(info, io.BytesIO(data))
            self.assertEqual(
                release.bundle_legal_files(archive),
                [("jdk/ASSEMBLY_EXCEPTION", b"ae"), ("jdk/legal/java.base/aes.md", b"aes"), ("tools/LICENSE.txt", b"l")],
            )


class NotesTest(unittest.TestCase):
    INFO = dict(
        version="0.2.0", commit=HEAD, commit_date="2026-10-26T18:40:41+00:00", compatibility="9.3.0", glibc="2.39",
        rustc="rustc 1.99.0", archive="bazel-rs-0.2.0-x86_64-unknown-linux-gnu.tar.gz", archive_sha256="f" * 64,
        gh_repo="o/r", prerelease=True, previous_tag="v0.1.0",
    )

    def test_classify_and_changelog(self):
        self.assertEqual(release.classify("parity(aspects): x"), "parity")
        self.assertEqual(release.classify("harden: x"), "fix")
        self.assertEqual(release.classify("feat!: x"), "feat")
        self.assertEqual(release.classify("integration: x"), "other")
        self.assertEqual(release.classify("Implement x"), "other")
        log = [("1" * 40, "perf: faster"), ("2" * 40, "fix: crash"), ("3" * 40, "release: bump version to 0.2.0"),
               ("4" * 40, "feat(query): new"), ("5" * 40, "Misc")]
        text = release.changelog_from_log(log)
        self.assertEqual(
            text,
            "### Features\n\n- feat(query): new (444444444)\n\n### Performance\n\n- perf: faster (111111111)\n\n"
            "### Fixes\n\n- fix: crash (222222222)\n\n### Other changes\n\n- Misc (555555555)\n",
        )
        self.assertEqual(release.changelog_from_log([]), "No changes.\n")

    def test_first_release_summary(self):
        plan = "# P\n\n## Status (today)\n\nMeasured (see `artifacts/RELEASES.log` for each release):\n\n- [x] a\n\n## Remaining\n\nno\n"
        self.assertEqual(release.first_release_summary(plan), "First release. Status at this commit (from PLAN.md):\n\nMeasured:\n\n- [x] a\n")
        self.assertIn("- [x]", release.first_release_summary((ROOT / "PLAN.md").read_text()))

    def test_previous_tag(self):
        runner = FakeRunner({("tag", "--merged", HEAD, "--list", "v[0-9]*"): Result(0, "v0.1.0\nv0.10.0\nv0.2.0\nv0.9.1\nvbogus\n")})
        self.assertEqual(release.previous_tag(runner, Path("/r"), HEAD, "0.10.0"), "v0.9.1")
        self.assertIsNone(release.previous_tag(runner, Path("/r"), HEAD, "0.1.0"))

    def test_render(self):
        info = release.NotesInfo(**self.INFO)
        text = release.render_notes(info, "### Fixes\n\n- fix: x (1)\n", prepend="Read me first.\n")
        self.assertTrue(text.startswith("Read me first.\n\n# bazel-rs 0.2.0\n"))
        for needle in (
            f"| Commit | `{HEAD}` (2026-10-26T18:40:41+00:00) |",
            "| Compatibility target | Bazel 9.3.0 |",
            "glibc ≥ 2.39",
            "gh release download v0.2.0 --repo o/r --pattern 'bazel-rs-0.2.0-x86_64-unknown-linux-gnu.tar.gz*'",
            "sha256sum -c bazel-rs-0.2.0-x86_64-unknown-linux-gnu.tar.gz.sha256",
            "install -m 0755 bazel-rs-0.2.0-x86_64-unknown-linux-gnu/bazel-rs",
            "https://github.com/o/r/blob/v0.2.0/PLAN.md",
            "## Changes since v0.1.0\n\n### Fixes",
        ):
            self.assertIn(needle, text)

    def test_recovery_commands(self):
        self.assertEqual(release.recovery_commands("tagged", "v1.0.0", "o/r"), ["git tag -d v1.0.0"])
        self.assertEqual(
            release.recovery_commands("released", "v1.0.0", "o/r"),
            ["gh release delete v1.0.0 --repo o/r --yes", "git push origin :refs/tags/v1.0.0", "git tag -d v1.0.0"],
        )


class BumpVersionTest(unittest.TestCase):
    def files(self):
        return {path: (ROOT / path).read_text() for path in bump_version.EDITS}

    def test_bump_repository_files(self):
        files = self.files()
        old = release.cargo_toml_version(files["Cargo.toml"])
        new = bump_version.bump(files, old, "9.8.7")
        self.assertEqual(release.cargo_toml_version(new["Cargo.toml"]), "9.8.7")
        self.assertEqual(release.cargo_lock_version(new["Cargo.lock"]), "9.8.7")
        for path, text in new.items():
            self.assertEqual(len(text.splitlines()), len(files[path].splitlines()), path)
        self.assertIn("bazel-rs 9.8.7 (experimental", new["tests/fixtures/module_extension_io/native_capture.py"])
        # Dependencies with the same version string are untouched.
        self.assertEqual(new["Cargo.lock"].count('version = "9.8.7"'), 1)

    def test_bump_needs_the_old_version(self):
        with self.assertRaises(release.ReleaseError):
            bump_version.bump(self.files(), "7.7.7", "9.8.7")


if __name__ == "__main__":
    unittest.main()
