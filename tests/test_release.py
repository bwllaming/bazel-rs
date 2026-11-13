"""Offline tests of scripts/release.py and scripts/bump_version.py (pure parts only).

No cargo, network or gh: git and gh are fake runners, except TagNotesGitTest,
which runs real git in a temporary directory (a local bare "origin").
"""

import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
import unittest.mock


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
        self.assertEqual(
            release.archive_stem("0.2.0", "x86_64-unknown-linux-musl"), "bazel-rs-0.2.0-x86_64-unknown-linux-musl"
        )
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
        line = "bazel-rs 1.2.3 (experimental; compatibility target Bazel 9.3.1)\n"
        self.assertEqual(release.check_version_output(line, "1.2.3"), "9.3.1")
        with self.assertRaises(release.ReleaseError):
            release.check_version_output(line, "1.2.4")
        with self.assertRaises(release.ReleaseError):
            release.check_version_output("Build label: 9.3.1\n", "1.2.3")


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
            base = {"PATH": "/bin", "RUSTFLAGS": "-Cdebuginfo=2", "CARGO_TARGET_DIR": "/elsewhere", "RUSTUP_TOOLCHAIN": "nightly", "CARGO_HOME": "/ch", "HOME": "/h"}
            env = release.build_env(base, repo, Path("/w/src"), Path("/w/target"), 42)
        self.assertEqual(env["RUSTUP_HOME"], str(repo / ".direnv" / "rustup"))
        self.assertEqual(env["CARGO_HOME"], "/ch")
        self.assertEqual(env["CARGO_TARGET_DIR"], "/w/target")
        self.assertEqual(env["SOURCE_DATE_EPOCH"], "42")
        self.assertNotIn("RUSTUP_TOOLCHAIN", env)
        self.assertEqual(
            env["RUSTFLAGS"],
            "--remap-path-prefix=/w/src=/bazel-rs --remap-path-prefix=/ch=/cargo "
            f"--remap-path-prefix={repo / '.direnv' / 'rustup'}=/rustup",
        )

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


def elf64(phdrs):
    """A minimal ELF64 LE header plus program headers [(p_type, payload bytes)]."""
    phoff, phentsize = 64, 56
    data_at = phoff + phentsize * len(phdrs)
    header = bytearray(64)
    header[:7] = b"\x7fELF\x02\x01\x01"
    header[18:20] = (62).to_bytes(2, "little")
    header[32:40] = phoff.to_bytes(8, "little")
    header[54:56] = phentsize.to_bytes(2, "little")
    header[56:58] = len(phdrs).to_bytes(2, "little")
    table, payload = bytearray(), bytearray()
    for kind, body in phdrs:
        ph = bytearray(phentsize)
        ph[0:4] = kind.to_bytes(4, "little")
        ph[8:16] = (data_at + len(payload)).to_bytes(8, "little")
        ph[32:40] = len(body).to_bytes(8, "little")
        table += ph
        payload += body
    return bytes(header + table + payload)


def dynamic(*tags):
    return b"".join(t.to_bytes(8, "little") + bytes(8) for t in (*tags, 0))


class TargetsTest(unittest.TestCase):
    def test_target_table(self):
        self.assertEqual(
            [t.triple for t in release.select_targets(None)],
            ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"],
        )
        self.assertEqual(
            [t.triple for t in release.select_targets(["aarch64-unknown-linux-musl", "x86_64-unknown-linux-gnu"])],
            ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-musl"],
        )
        with self.assertRaises(release.ReleaseError):
            release.select_targets(["riscv64gc-unknown-linux-gnu"])
        arm = release.TARGETS_BY_TRIPLE["aarch64-unknown-linux-musl"]
        self.assertTrue(arm.static and arm.default and not arm.caveat)
        self.assertEqual((arm.runner, arm.cpu, arm.bundle, arm.platform, arm.kernel),
                         ("ubuntu-24.04-arm", "aarch64", "linux-arm64", "Linux arm64", "4.1"))
        self.assertIn(("JEMALLOC_SYS_WITH_LG_PAGE", "16"), arm.env)
        for t in release.TARGETS:
            # Every target's embedded tools bundle is checked in.
            self.assertTrue((ROOT / "resources/bazel_tools" / t.bundle / "embedded_tools.tar.gz").is_file(), t.bundle)
        toolchain = (ROOT / "rust-toolchain.toml").read_text()
        for t in release.TARGETS:
            if t.zig:
                self.assertIn(f'"{t.triple}"', toolchain)

    def test_zig_wrapper_and_cross_env(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            fake = tmp / "zig dir" / "zig"
            fake.parent.mkdir()
            fake.write_text('#!/bin/sh\nprintf "%s|" "$@"\n')
            fake.chmod(0o755)
            arm = release.TARGETS_BY_TRIPLE["aarch64-unknown-linux-musl"]
            env = release.cross_env(arm, str(fake), tmp / "tools", tmp / "cache")
            self.assertEqual(env["CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER"], "rust-lld")
            self.assertEqual(env["JEMALLOC_SYS_WITH_LG_PAGE"], "16")
            self.assertEqual(env["ZIG_GLOBAL_CACHE_DIR"], str(tmp / "cache"))
            cc = env["CC_aarch64_unknown_linux_musl"]
            out = release.run_command([cc, "--target=aarch64-unknown-linux-musl", "-O2", "a b.c"]).stdout
            self.assertEqual(out, "cc|-target|aarch64-linux-musl|-fno-sanitize=all|-O2|a b.c|")
            out = release.run_command([env["AR_aarch64_unknown_linux_musl"], "crs", "x.a"]).stdout
            self.assertEqual(out, "ar|crs|x.a|")
            musl = release.TARGETS_BY_TRIPLE["x86_64-unknown-linux-musl"]
            env = release.cross_env(musl, str(fake), tmp / "tools", tmp / "cache")
            self.assertFalse(any(k.startswith("CARGO_TARGET_") for k in env))
            self.assertNotIn("JEMALLOC_SYS_WITH_LG_PAGE", env)

    def test_find_zig(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            self.assertIsNone(release.find_zig(None, repo, {"PATH": ""}))
            zig = repo / ".direnv" / "zig" / "zig"
            zig.parent.mkdir(parents=True)
            zig.write_text("#!/bin/sh\n")
            zig.chmod(0o755)
            self.assertEqual(release.find_zig(None, repo, {"PATH": ""}), str(zig.resolve()))
            self.assertEqual(release.find_zig(None, repo, {"ZIG": "/bin/sh", "PATH": ""}), str(Path("/bin/sh").resolve()))

    def test_elf_linkage(self):
        with tempfile.TemporaryDirectory() as tmp:
            elf = Path(tmp) / "bin"
            elf.write_bytes(elf64([(1, b""), (2, dynamic(30, 0x6FFFFEF5))]))  # static-pie: PT_DYNAMIC, no DT_NEEDED
            self.assertEqual(release.elf_linkage(elf), (False, 0))
            static = release.TARGETS_BY_TRIPLE["x86_64-unknown-linux-musl"]
            self.assertIn("statically linked", release.check_linkage(FakeRunner(), static, elf))
            elf.write_bytes(elf64([(3, b"/lib64/ld-linux-x86-64.so.2\0"), (2, dynamic(1, 1, 30))]))
            self.assertEqual(release.elf_linkage(elf), (True, 2))
            with self.assertRaisesRegex(release.ReleaseError, "is not static"):
                release.check_linkage(FakeRunner(), static, elf)
            with self.assertRaisesRegex(release.ReleaseError, "arm64"):
                release.check_linkage(FakeRunner(), release.TARGETS_BY_TRIPLE["aarch64-unknown-linux-musl"], elf)
            elf.write_bytes(b"#!/bin/sh\n" + bytes(60))
            with self.assertRaises(release.ReleaseError):
                release.elf_linkage(elf)
        if Path("/bin/sh").is_file() and release.elf_machine(Path("/bin/sh")) is not None:
            interp, needed = release.elf_linkage(Path("/bin/sh").resolve())
            self.assertTrue(interp and needed >= 1)

    def test_smoke_plan(self):
        gnu = release.TARGETS_BY_TRIPLE["x86_64-unknown-linux-gnu"]
        arm = release.TARGETS_BY_TRIPLE["aarch64-unknown-linux-musl"]
        no_binfmt = lambda machine: False
        self.assertEqual(release.smoke_plan(gnu, None, False, release.EM_X86_64, no_binfmt), ([], True, release.SMOKE_FULL))
        self.assertEqual(release.smoke_plan(arm, None, False, release.EM_AARCH64, no_binfmt)[:2], ([], True))
        self.assertEqual(release.smoke_plan(arm, None, False, release.EM_X86_64, lambda m: True)[:2], ([], True))
        self.assertEqual(
            release.smoke_plan(arm, "/q/qemu-aarch64", False, release.EM_X86_64, no_binfmt),
            (["/q/qemu-aarch64"], False, release.SMOKE_QEMU),
        )
        self.assertEqual(release.smoke_plan(arm, None, True, release.EM_X86_64, no_binfmt), (None, False, release.SMOKE_NONE))
        with self.assertRaisesRegex(release.ReleaseError, "--qemu-aarch64"):
            release.smoke_plan(arm, None, False, release.EM_X86_64, no_binfmt)

    def test_binfmt_runs(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "status").write_text("enabled\n")
            (root / "register").write_text("")
            self.assertFalse(release.binfmt_runs(release.EM_AARCH64, root))
            (root / "qemu-aarch64").write_text(
                "enabled\ninterpreter /usr/bin/qemu-aarch64-static\nflags: F\noffset 0\n"
                "magic 7f454c460201010000000000000000000200b700\nmask ffffffffffffff00fffffffffffffffffeffffff\n"
            )
            self.assertTrue(release.binfmt_runs(release.EM_AARCH64, root))
            self.assertFalse(release.binfmt_runs(release.EM_X86_64, root))
            (root / "qemu-aarch64").write_text("disabled\nmagic 7f454c460201010000000000000000000200b700\n")
            self.assertFalse(release.binfmt_runs(release.EM_AARCH64, root))

    def test_smoke_env(self):
        env = release.smoke_env(Path("/s"))
        # The smoke build reads the output path from the result line.
        self.assertEqual(env["BAZEL_RS_RESULT_LINES"], "1")
        self.assertEqual(env["HOME"], "/s/home")
        self.assertEqual(env["https_proxy"], release.DEAD_PROXY)
        online = release.smoke_env(Path("/s"), network=True)
        self.assertFalse(any("proxy" in key.lower() for key in online))
        self.assertEqual(online["HOME"], "/s/home")

    def test_network_smoke_checks_the_host_platform(self):
        with tempfile.TemporaryDirectory() as tmp:
            scratch = Path(tmp)
            out = scratch / "host.txt"
            calls = []

            def runner(argv, **kwargs):
                calls.append((list(map(str, argv)), kwargs))
                return Result(0, f"//:host\t{out}\n")

            out.write_text("aarch64 @platforms//cpu:aarch64,@platforms//os:linux\n")
            release.network_smoke_test(runner, Path("/b/bazel-rs"), scratch, ["/q"], "aarch64")
            argv, kwargs = calls[0]
            self.assertEqual(argv[:2], ["/q", "/b/bazel-rs"])
            self.assertIn("build", argv)
            self.assertNotIn("--experimental-builtin-modules=false", argv)
            self.assertNotIn("https_proxy", kwargs["env"])
            self.assertIn('bazel_dep(name = "platforms"', (scratch / "ws-network" / "MODULE.bazel").read_text())
            self.assertIn("HOST_CONSTRAINTS", (scratch / "ws-network" / "BUILD.bazel").read_text())
            with self.assertRaisesRegex(release.ReleaseError, "detected 'aarch64 @platforms"):
                release.network_smoke_test(runner, Path("/b/bazel-rs"), scratch, [], "x86_64")

    def test_default_temp_root(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            self.assertEqual(release.default_temp_root({"TMPDIR": "/t", "HOME": str(home)}), Path("/t"))
            self.assertIsNone(release.default_temp_root({"HOME": str(home)}))
            (home / "opencodetmp").mkdir()
            self.assertEqual(release.default_temp_root({"HOME": str(home)}), home / "opencodetmp")


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
        self.assertIn("`x86_64-unknown-linux-gnu` (dev-dependencies excluded)", text)
        self.assertNotIn("Statically linked C runtime", text)

    def test_markdown_static_runtime(self):
        text = release.third_party_markdown(
            "0.1.0", HEAD, release.runtime_packages(self.METADATA), lambda p: [], b"bazel", [],
            "x86_64-unknown-linux-musl", [("musl libc (MIT)", b"musl text"), ("LLVM libunwind", b"llvm text")],
        )
        self.assertIn("`x86_64-unknown-linux-musl` (dev-dependencies excluded)", text)
        self.assertIn("## Statically linked C runtime", text)
        self.assertIn("musl libc (MIT):\n\n```text\nmusl text\n```", text)

    def test_static_runtime_licenses(self):
        with tempfile.TemporaryDirectory() as tmp:
            zig = Path(tmp) / "zig"
            zig.write_text("")
            with self.assertRaisesRegex(release.ReleaseError, "COPYRIGHT"):
                release.static_runtime_licenses(str(zig))
            for _, relative in release.STATIC_RUNTIME_LICENSES:
                (Path(tmp) / relative).parent.mkdir(parents=True, exist_ok=True)
                (Path(tmp) / relative).write_text(relative)
            found = release.static_runtime_licenses(str(zig))
            self.assertEqual([t for t, _ in found], [t for t, _ in release.STATIC_RUNTIME_LICENSES])
            self.assertEqual(found[0][1], b"lib/libc/musl/COPYRIGHT")

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
    GNU = dict(
        triple="x86_64-unknown-linux-gnu", arch="x86_64", archive="bazel-rs-0.2.0-x86_64-unknown-linux-gnu.tar.gz",
        sha256="f" * 64, requirement="glibc ≥ 2.39", tested=release.SMOKE_FULL,
    )
    MUSL = dict(
        triple="x86_64-unknown-linux-musl", arch="x86_64", archive="bazel-rs-0.2.0-x86_64-unknown-linux-musl.tar.gz",
        sha256="e" * 64, requirement="statically linked (musl libc, jemalloc): no glibc or other shared library needed",
        tested=release.SMOKE_FULL, static=True,
    )
    ARM = dict(
        triple="aarch64-unknown-linux-musl", arch="arm64", archive="bazel-rs-0.2.0-aarch64-unknown-linux-musl.tar.gz",
        sha256="d" * 64, requirement="statically linked", tested=release.SMOKE_FULL, static=True,
        platform="Linux arm64", kernel="4.1",
    )

    def info(self, *assets):
        return release.NotesInfo(
            version="0.2.0", commit=HEAD, commit_date="2026-10-26T18:40:41+00:00", compatibility="9.3.1",
            rustc="rustc 1.99.0", assets=[release.AssetInfo(**a) for a in assets], gh_repo="o/r", prerelease=True,
            previous_tag="v0.1.0",
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
        info = self.info(self.GNU)
        text = release.render_notes(info, "### Fixes\n\n- fix: x (1)\n", prepend="Read me first.\n")
        self.assertTrue(text.startswith("Read me first.\n\n# bazel-rs 0.2.0\n"))
        for needle in (
            f"| Commit | `{HEAD}` (2026-10-26T18:40:41+00:00) |",
            "| Compatibility target | Bazel 9.3.1 |",
            "glibc ≥ 2.39",
            "curl -fLO https://github.com/o/r/releases/download/v0.2.0/bazel-rs-0.2.0-x86_64-unknown-linux-gnu.tar.gz -O https://github.com/o/r/releases/download/v0.2.0/bazel-rs-0.2.0-x86_64-unknown-linux-gnu.tar.gz.sha256",
            "sha256sum -c bazel-rs-0.2.0-x86_64-unknown-linux-gnu.tar.gz.sha256",
            "install -m 0755 bazel-rs-0.2.0-x86_64-unknown-linux-gnu/bazel-rs",
            "https://github.com/o/r/blob/v0.2.0/PLAN.md",
            "## Changes since v0.1.0\n\n### Fixes",
        ):
            self.assertIn(needle, text)
        self.assertNotIn("prefer the static one", text)

    def test_render_static_and_arm64(self):
        text = release.render_notes(self.info(self.GNU, self.MUSL, self.ARM), "")
        for needle in (
            f"| `bazel-rs-0.2.0-x86_64-unknown-linux-gnu.tar.gz` | Linux x86_64 (`x86_64-unknown-linux-gnu`) | glibc ≥ 2.39 | {release.SMOKE_FULL} | `{'f' * 64}` |",
            f"| `bazel-rs-0.2.0-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64 (`x86_64-unknown-linux-musl`) |",
            "- `x86_64-unknown-linux-musl`: any Linux x86_64 distribution (kernel 3.2 or newer); statically linked, no glibc needed.",
            "- `x86_64-unknown-linux-gnu`: Linux x86_64 with glibc ≥ 2.39 (e.g. Ubuntu 24.04 or newer).",
            f"| `bazel-rs-0.2.0-aarch64-unknown-linux-musl.tar.gz` | Linux arm64 (`aarch64-unknown-linux-musl`) | statically linked | {release.SMOKE_FULL} |",
            "- `aarch64-unknown-linux-musl`: any Linux arm64 distribution (kernel 4.1 or newer); statically linked, no glibc needed.",
            "prefer the static one",
            # The install steps pick this machine's static archive.
            "stem=bazel-rs-0.2.0-$(uname -m)-unknown-linux-musl   # x86_64 or aarch64",
            'curl -fLO "https://github.com/o/r/releases/download/v0.2.0/$stem.tar.gz" -O "https://github.com/o/r/releases/download/v0.2.0/$stem.tar.gz.sha256"',
            'sha256sum -c "$stem.tar.gz.sha256"',
            'install -m 0755 "$stem/bazel-rs" ~/.local/bin/bazel-rs',
        ):
            self.assertIn(needle, text)
        self.assertNotIn("Not for general use", text)
        self.assertEqual(release.recommended_asset(self.info(self.ARM, self.GNU).assets).triple, "x86_64-unknown-linux-gnu")
        # One static archive: its explicit name.
        text = release.render_notes(self.info(self.GNU, self.MUSL), "")
        self.assertIn("curl -fLO https://github.com/o/r/releases/download/v0.2.0/bazel-rs-0.2.0-x86_64-unknown-linux-musl.tar.gz -O https://github.com/o/r/releases/download/v0.2.0/bazel-rs-0.2.0-x86_64-unknown-linux-musl.tar.gz.sha256", text)
        self.assertNotIn("uname", text)

    def test_recovery_commands(self):
        self.assertEqual(release.recovery_commands("tagged", "v1.0.0", "o/r"), ["git tag -d v1.0.0"])
        self.assertEqual(
            release.recovery_commands("pushed", "v1.0.0", "o/r"),
            ["git push origin :refs/tags/v1.0.0", "git tag -d v1.0.0"],
        )
        self.assertEqual(
            release.recovery_commands("released", "v1.0.0", "o/r"),
            ["gh release delete v1.0.0 --repo o/r --yes", "git push origin :refs/tags/v1.0.0", "git tag -d v1.0.0"],
        )
        # CI never touches the tag.
        self.assertEqual(release.recovery_commands("released", "v1.0.0", "o/r", ci=True), ["gh release delete v1.0.0 --repo o/r --yes"])
        self.assertEqual(release.recovery_commands("none", "v1.0.0", "o/r", ci=True), [])

    def test_build_url_row(self):
        info = self.info(self.MUSL)
        self.assertNotIn("Built by", release.render_notes(info, ""))
        self.assertNotIn("## Changes", release.render_notes(info, ""))
        info.build_url = "https://github.com/o/r/actions/runs/42"
        self.assertIn("| Toolchain | rustc 1.99.0 |\n| Built by | [GitHub Actions run](https://github.com/o/r/actions/runs/42) |\n", release.render_notes(info, ""))


TAGGED = "refs/tags/v1.2.3"
GH_RUN_LIST = (
    "gh", "run", "list", "--repo", "o/r", "--workflow", "release.yml", "--event", "push",
    "--branch", "v1.2.3", "--limit", "1", "--json", "databaseId,url,headSha,status",
)
RUN_ENTRY = {"databaseId": 77, "url": "https://github.com/o/r/actions/runs/77", "headSha": HEAD, "status": "queued"}


class SequenceRunner(FakeRunner):
    """FakeRunner whose answers may be lists, consumed one call at a time."""

    def __call__(self, argv, **kwargs):
        key = self.key(argv)
        self.calls.append(key)
        if key not in self.answers:
            raise AssertionError(f"unexpected command {key}")
        answer = self.answers[key]
        return answer.pop(0) if isinstance(answer, list) else answer


def tag_argv(message):
    return ("tag", "-a", "--cleanup=verbatim", "-m", message, "v1.2.3", HEAD)


PUSH = ("push", "origin", f"{TAGGED}:{TAGGED}")


class TagFlowTest(unittest.TestCase):
    def runner(self, message, overrides=None):
        answers = {tag_argv(message): Result(0), PUSH: Result(0), GH_RUN_LIST: Result(0, json.dumps([RUN_ENTRY]))}
        answers.update(overrides or {})
        return SequenceRunner(answers)

    def test_tag_push_and_run_url(self):
        message = release.tag_message("1.2.3", "## Highlights\n\n- x\n")
        runner = self.runner(message)
        sleeps = []
        out = io.StringIO()
        with unittest.mock.patch("sys.stdout", out):
            code = release.push_tag(
                runner, repo_dir=Path("/repo"), gh="/x/gh", gh_repo="o/r", version="1.2.3", commit=HEAD,
                message=message, sleep=sleeps.append,
            )
        self.assertEqual(code, 0)
        # Annotated tag, then ONLY the tag is pushed, then the run is looked up; no release is created locally.
        self.assertEqual(runner.calls, [tag_argv(message), PUSH, GH_RUN_LIST])
        self.assertEqual(sleeps, [])
        self.assertIn("workflow run: https://github.com/o/r/actions/runs/77", out.getvalue())
        self.assertIn("gh run watch 77 --repo o/r --exit-status", out.getvalue())

    def test_polls_until_the_run_of_the_commit_appears(self):
        other = dict(RUN_ENTRY, headSha="b" * 40)
        message = release.tag_message("1.2.3")
        runner = self.runner(message, {GH_RUN_LIST: [Result(0, "[]"), Result(1, "", "HTTP 502"), Result(0, json.dumps([other])), Result(0, json.dumps([RUN_ENTRY]))]})
        sleeps = []
        with unittest.mock.patch("sys.stdout", io.StringIO()):
            found = release.find_run(runner, "/x/gh", "o/r", "v1.2.3", HEAD, sleep=sleeps.append)
        self.assertEqual(found, RUN_ENTRY)
        self.assertEqual(sleeps, [5.0, 5.0, 5.0])
        never = self.runner(message, {GH_RUN_LIST: Result(0, "[]")})
        self.assertIsNone(release.find_run(never, "/x/gh", "o/r", "v1.2.3", HEAD, attempts=3, sleep=lambda s: None))
        self.assertEqual(never.calls.count(GH_RUN_LIST), 3)

    def test_run_not_found(self):
        message = release.tag_message("1.2.3")
        for watch, want in ((False, 0), (True, 1)):
            runner = self.runner(message, {GH_RUN_LIST: Result(0, "[]")})
            with unittest.mock.patch("sys.stdout", io.StringIO()), unittest.mock.patch("sys.stderr", io.StringIO()) as err:
                code = release.push_tag(
                    runner, repo_dir=Path("/repo"), gh="/x/gh", gh_repo="o/r", version="1.2.3", commit=HEAD,
                    message=message, watch=watch, sleep=lambda s: None,
                )
            self.assertEqual(code, want)
            self.assertIn("https://github.com/o/r/actions/workflows/release.yml", err.getvalue())

    def test_watch(self):
        message = release.tag_message("1.2.3")
        watch = ("gh", "run", "watch", "77", "--repo", "o/r", "--exit-status", "--interval", "30")
        view = ("gh", "release", "view", "v1.2.3", "--repo", "o/r", "--json", "url")
        runner = self.runner(message, {watch: Result(0), view: Result(0, '{"url": "https://github.com/o/r/releases/tag/v1.2.3"}')})
        with unittest.mock.patch("sys.stdout", io.StringIO()) as out:
            code = release.push_tag(
                runner, repo_dir=Path("/repo"), gh="/x/gh", gh_repo="o/r", version="1.2.3", commit=HEAD,
                message=message, watch=True,
            )
        self.assertEqual(code, 0)
        self.assertEqual(runner.calls, [tag_argv(message), PUSH, GH_RUN_LIST, watch, view])
        self.assertIn("published https://github.com/o/r/releases/tag/v1.2.3", out.getvalue())
        failed = self.runner(message, {watch: Result(1)})
        with unittest.mock.patch("sys.stdout", io.StringIO()), unittest.mock.patch("sys.stderr", io.StringIO()) as err:
            code = release.push_tag(
                failed, repo_dir=Path("/repo"), gh="/x/gh", gh_repo="o/r", version="1.2.3", commit=HEAD,
                message=message, watch=True,
            )
        self.assertEqual(code, 1)
        self.assertIn("gh workflow run release.yml --repo o/r -f tag=v1.2.3", err.getvalue())

    def test_failed_push_prints_recovery(self):
        message = release.tag_message("1.2.3")
        runner = self.runner(message)

        def fail_push(argv, **kwargs):
            if runner.key(argv) == PUSH:
                raise release.ReleaseError("push rejected")
            return runner(argv, **kwargs)

        with unittest.mock.patch("sys.stderr", io.StringIO()) as err, self.assertRaises(release.ReleaseError):
            release.push_tag(
                fail_push, repo_dir=Path("/repo"), gh="/x/gh", gh_repo="o/r", version="1.2.3", commit=HEAD, message=message,
            )
        self.assertIn("stage: tagged", err.getvalue())
        self.assertIn("git tag -d v1.2.3", err.getvalue())
        self.assertNotIn("git push origin :", err.getvalue())


class TagNotesTest(unittest.TestCase):
    def test_message_and_round_trip(self):
        self.assertEqual(release.tag_message("1.2.3"), "bazel-rs 1.2.3\n")
        notes = "## Highlights\n\n# not a comment\n- x\n"
        message = release.tag_message("1.2.3", "\n" + notes + "\n\n")
        self.assertEqual(message, "bazel-rs 1.2.3\n\n" + notes)
        body = message.split("\n\n", 1)[1]
        self.assertEqual(release.notes_from_tag_body(body), (notes, False))
        replaced = release.tag_message("1.2.3", notes, replace_changelog=True)
        self.assertEqual(release.notes_from_tag_body(replaced.split("\n\n", 1)[1]), (notes, True))
        self.assertEqual(release.notes_from_tag_body(""), (None, False))
        self.assertEqual(release.notes_from_tag_body(release.REPLACE_CHANGELOG_MARKER + "\n"), (None, True))

    def test_ci_run_url(self):
        env = {"GITHUB_SERVER_URL": "https://github.com", "GITHUB_REPOSITORY": "o/r", "GITHUB_RUN_ID": "42"}
        self.assertEqual(release.ci_run_url(env), "https://github.com/o/r/actions/runs/42")
        self.assertEqual(release.ci_run_url({**env, "GITHUB_RUN_ATTEMPT": "1"}), "https://github.com/o/r/actions/runs/42")
        self.assertEqual(release.ci_run_url({**env, "GITHUB_RUN_ATTEMPT": "3"}), "https://github.com/o/r/actions/runs/42/attempts/3")
        self.assertIsNone(release.ci_run_url({"GITHUB_RUN_ID": "42"}))


def ci_answers(overrides=None):
    answers = {
        ("fetch", "--quiet", "--force", "--no-tags", "origin", f"+{TAGGED}:{TAGGED}"): Result(0),
        ("rev-parse", "--verify", "HEAD^{commit}"): Result(0, HEAD + "\n"),
        ("rev-parse", "--verify", "--quiet", f"{TAGGED}^{{commit}}"): Result(0, HEAD + "\n"),
        ("status", "--porcelain", "--untracked-files=normal"): Result(0, ""),
        ("show", f"{HEAD}:Cargo.toml"): Result(0, CARGO_TOML),
        ("show", f"{HEAD}:Cargo.lock"): Result(0, CARGO_LOCK),
        ("cat-file", "-t", TAGGED): Result(0, "tag\n"),
        ("for-each-ref", "--format=%(contents:body)", TAGGED): Result(0, "Extra notes.\n"),
        ("gh", "auth", "status", "--hostname", "github.com"): Result(0),
        ("gh", "release", "view", "v1.2.3", "--repo", "o/r", "--json", "tagName"): Result(1, "", "release not found\n"),
    }
    answers.update(overrides or {})
    return FakeRunner(answers)


def run_ci_preflight(runner, version="1.2.3"):
    return release.ci_preflight(runner, version, repo_dir=Path("/repo"), gh="/x/gh", gh_repo="o/r")


class CiPreflightTest(unittest.TestCase):
    def test_tag_at_head(self):
        runner = ci_answers()
        problems, head, body = run_ci_preflight(runner)
        self.assertEqual((problems, head, body), ([], HEAD, "Extra notes.\n"))
        # No branch checks (detached HEAD is expected) and nothing written.
        for call in runner.calls:
            self.assertNotIn("symbolic-ref", call)
            self.assertNotIn("push", call)
            self.assertNotIn("create", call)
            self.assertNotEqual(call[:1], ("tag",))

    def assertProblem(self, runner, needle, version="1.2.3"):
        problems, _, _ = run_ci_preflight(runner, version)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn(needle, problems[0])

    def test_tag_problems(self):
        self.assertProblem(
            ci_answers({("rev-parse", "--verify", "--quiet", f"{TAGGED}^{{commit}}"): Result(0, "b" * 40 + "\n")}),
            "is commit bbbbbbbbbbbb, HEAD is aaaaaaaaaaaa",
        )
        missing = ci_answers({
            ("rev-parse", "--verify", "--quiet", f"{TAGGED}^{{commit}}"): Result(1),
            ("fetch", "--quiet", "--force", "--no-tags", "origin", f"+{TAGGED}:{TAGGED}"): Result(128, "", "couldn't find remote ref"),
        })
        problems, _, body = run_ci_preflight(missing)
        self.assertEqual(len(problems), 2, problems)
        self.assertIn("cannot fetch tag v1.2.3", problems[0])
        self.assertIn("never creates tags", problems[1])
        self.assertEqual(body, "")

    def test_version_dirty_and_release(self):
        self.assertProblem(ci_answers({("status", "--porcelain", "--untracked-files=normal"): Result(0, " M Cargo.toml\n")}), "not clean")
        self.assertProblem(
            ci_answers({("gh", "release", "view", "v1.2.3", "--repo", "o/r", "--json", "tagName"): Result(0, "{}")}),
            "already exists",
        )
        tag2 = "refs/tags/v2.0.0"
        runner = ci_answers({
            ("fetch", "--quiet", "--force", "--no-tags", "origin", f"+{tag2}:{tag2}"): Result(0),
            ("rev-parse", "--verify", "--quiet", f"{tag2}^{{commit}}"): Result(0, HEAD + "\n"),
            ("cat-file", "-t", tag2): Result(0, "tag\n"),
            ("for-each-ref", "--format=%(contents:body)", tag2): Result(0, ""),
            ("gh", "release", "view", "v2.0.0", "--repo", "o/r", "--json", "tagName"): Result(1, "", "release not found"),
        })
        problems, _, _ = run_ci_preflight(runner, "2.0.0")
        self.assertEqual(len(problems), 2, problems)
        self.assertIn("Cargo.toml says 1.2.3, releasing 2.0.0", problems[0])

    def test_lightweight_tag(self):
        runner = ci_answers({("cat-file", "-t", TAGGED): Result(0, "commit\n")})
        with unittest.mock.patch("sys.stderr", io.StringIO()) as err:
            problems, _, body = run_ci_preflight(runner)
        self.assertEqual((problems, body), ([], ""))
        self.assertIn("lightweight", err.getvalue())


class TagNotesGitTest(unittest.TestCase):
    """The tag flow's message survives git verbatim and --ci reads it back (real git)."""

    def git(self, *args, cwd):
        env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
        return subprocess.run(["git", *args], cwd=cwd, env=env, check=True, capture_output=True, text=True).stdout

    def test_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            origin, work, ci = tmp / "origin.git", tmp / "work", tmp / "ci"
            self.git("init", "--quiet", "--bare", str(origin), cwd=tmp)
            self.git("init", "--quiet", "-b", "main", str(work), cwd=tmp)
            (work / "Cargo.toml").write_text(CARGO_TOML)
            (work / "Cargo.lock").write_text(CARGO_LOCK)
            for args in (("add", "."), ("-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "release: 1.2.3"),
                         ("remote", "add", "origin", str(origin)), ("push", "--quiet", "origin", "main")):
                self.git(*args, cwd=work)
            commit = self.git("rev-parse", "HEAD", cwd=work).strip()
            notes = "## Highlights\n\n# A heading the default cleanup would drop\n\n- item\n"
            message = release.tag_message("1.2.3", notes, replace_changelog=True)
            env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
                   "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}

            def run(argv, **kwargs):
                kwargs.setdefault("env", env)
                kwargs.pop("echo", None)
                return release.run_command(argv, **kwargs)

            gh_list = ("gh", *GH_RUN_LIST[1:])
            with unittest.mock.patch("sys.stdout", io.StringIO()), unittest.mock.patch("sys.stderr", io.StringIO()):
                code = release.push_tag(
                    lambda argv, **kw: Result(1) if tuple(map(str, argv))[1:] == gh_list[1:] else run(argv, **kw),
                    repo_dir=work, gh="gh", gh_repo="o/r", version="1.2.3", commit=commit, message=message,
                    sleep=lambda s: None,
                )
            self.assertEqual(code, 0)
            # What the workflow sees: a detached checkout of the tag in another clone.
            self.git("clone", "--quiet", str(origin), str(ci), cwd=tmp)
            self.git("checkout", "--quiet", "--detach", "v1.2.3", cwd=ci)
            with unittest.mock.patch("sys.stderr", io.StringIO()):
                problems, head, body = release.ci_preflight(run, "1.2.3", repo_dir=ci, gh=None, gh_repo="o/r")
            self.assertEqual(problems, ["gh (GitHub CLI) not found: pass --gh PATH or set GH"])
            self.assertEqual(head, commit)
            self.assertEqual(release.notes_from_tag_body(body), (notes, True))


class StagesTest(unittest.TestCase):
    """--ci --stage build records and --stage publish collection."""

    def write_build(self, root, triple, *, version="1.2.3", commit=HEAD, compat="9.3.1", data=None):
        t = release.TARGETS_BY_TRIPLE[triple]
        root.mkdir(parents=True, exist_ok=True)
        archive = root / f"{release.archive_stem(version, triple)}.tar.gz"
        archive.write_bytes(data or triple.encode())
        info = release.AssetInfo(
            triple, t.arch, archive.name, release.sha256_file(archive), "req", release.SMOKE_FULL, t.static,
            t.caveat, t.platform, t.kernel,
        )
        record = release.asset_record(info, version=version, commit=commit, compatibility=compat, rustc="rustc 1.99.0")
        (root / f"{archive.name.removesuffix('.tar.gz')}{release.RECORD_SUFFIX}").write_text(json.dumps(record))
        return archive

    def test_records_round_trip_and_checks(self):
        targets = release.select_targets(None)
        with tempfile.TemporaryDirectory() as tmp:
            built = Path(tmp) / "built"
            self.write_build(built / "release-build-linux-x86_64", "x86_64-unknown-linux-gnu")
            self.write_build(built / "release-build-linux-x86_64", "x86_64-unknown-linux-musl")
            with self.assertRaisesRegex(release.ReleaseError, "missing aarch64-unknown-linux-musl"):
                release.load_records(built, version="1.2.3", commit=HEAD, targets=targets)
            arm = self.write_build(built / "release-build-linux-arm64", "aarch64-unknown-linux-musl")
            pairs, compat, rustc = release.load_records(built, version="1.2.3", commit=HEAD, targets=targets)
            self.assertEqual([info.triple for info, _ in pairs], [t.triple for t in targets])
            self.assertEqual((compat, rustc), ("9.3.1", "rustc 1.99.0"))
            self.assertEqual(pairs[2][0].platform, "Linux arm64")
            self.assertEqual(pairs[2][0].kernel, "4.1")
            # Only the selected targets may be present.
            with self.assertRaisesRegex(release.ReleaseError, "unexpected aarch64-unknown-linux-musl"):
                release.load_records(built, version="1.2.3", commit=HEAD, targets=targets[:2])
            with self.assertRaisesRegex(release.ReleaseError, "publishing 1.2.4"):
                release.load_records(built, version="1.2.4", commit=HEAD, targets=targets)
            arm.write_bytes(b"tampered")
            with self.assertRaisesRegex(release.ReleaseError, "sha256 differs"):
                release.load_records(built, version="1.2.3", commit=HEAD, targets=targets)
            self.write_build(built / "release-build-linux-arm64", "aarch64-unknown-linux-musl", compat="9.4.0")
            with self.assertRaisesRegex(release.ReleaseError, "different compatibility targets"):
                release.load_records(built, version="1.2.3", commit=HEAD, targets=targets)
            self.write_build(built / "again", "aarch64-unknown-linux-musl")
            with self.assertRaisesRegex(release.ReleaseError, "two build records"):
                release.load_records(built, version="1.2.3", commit=HEAD, targets=targets)

    def test_collect_release_writes_checksums_and_notes(self):
        targets = release.select_targets(None)
        runner = FakeRunner({
            ("show", "-s", "--format=%cI", HEAD): Result(0, "2026-10-28T00:00:00+00:00\n"),
            ("tag", "--merged", HEAD, "--list", "v[0-9]*"): Result(0, "v1.2.2\n"),
            ("log", "--no-merges", "--format=%H%x09%s", f"v1.2.2..{HEAD}"): Result(0, f"{'b' * 40}\tport(arm64): x\n"),
        })
        with tempfile.TemporaryDirectory() as tmp:
            built, work = Path(tmp) / "built", Path(tmp) / "work"
            for t in targets:
                self.write_build(built / t.runner, t.triple)
            result = release.collect_release(
                runner, repo_dir=Path("/repo"), version="1.2.3", commit=HEAD, gh_repo="o/r", prerelease=False,
                targets=targets, source=built, work=work, prepend=None, replace_changelog=False,
                build_url="https://github.com/o/r/actions/runs/1",
            )
            self.assertEqual([a.name for a in result.archives], [f"{release.archive_stem('1.2.3', t.triple)}.tar.gz" for t in targets])
            sums = (work / "dist" / "SHA256SUMS").read_text().splitlines()
            self.assertEqual(len(sums), 3)
            notes = result.notes.read_text()
            self.assertIn("Linux arm64 (`aarch64-unknown-linux-musl`)", notes)
            self.assertIn("| Built by | [GitHub Actions run](https://github.com/o/r/actions/runs/1) |", notes)
            self.assertIn("port(arm64): x", notes)
            self.assertEqual(len(result.assets), 3 + 3 + 1)


class WorkflowTest(unittest.TestCase):
    def test_release_workflow(self):
        text = (ROOT / ".github" / "workflows" / "release.yml").read_text()
        for needle in (
            'tags: ["v*"]',
            "workflow_dispatch:",
            "runs-on: ${{ matrix.runner }}",
            # The publish job runs on a GitHub-hosted runner.
            "runs-on: ubuntu-24.04\n",
            "contents: write",
            "contents: read",
            "fetch-depth: 0",
            "rustup toolchain install",
            "scripts/setup-zig.sh",
            "Swatinem/rust-cache",
            "GH_TOKEN: ${{ github.token }}",
            'scripts/release.py --ci --stage build --smoke-network "${args[@]}"',
            'scripts/release.py --ci --stage publish --from "$RUNNER_TEMP/built"',
            '--version "${TAG#v}"',
            "needs: build",
            "pattern: release-build-*",
            "timeout-minutes:",
            "concurrency:",
        ):
            self.assertIn(needle, text)
        # Only the publish job may write (create the release).
        self.assertEqual(text.count("contents: write"), 1)
        self.assertLess(text.index("publish:"), text.index("contents: write"))
        # The workflow never makes tags itself.
        self.assertNotIn("git tag", text.replace("# ", ""))
        self.assertNotIn("git push", text)
        self.assertEqual(release.WORKFLOW, "release.yml")

    def test_build_matrix_covers_every_default_target_on_its_runner(self):
        text = (ROOT / ".github" / "workflows" / "release.yml").read_text()
        import re
        entries = re.findall(r"- name: (\S+)\n\s+runner: (\S+)\n\s+targets: ([^\n]+)", text)
        self.assertGreaterEqual(len(entries), 2)
        built = {}
        for _, runner, targets in entries:
            for triple in targets.split():
                self.assertNotIn(triple, built)
                built[triple] = runner
        self.assertEqual(built, {t.triple: t.runner for t in release.TARGETS if t.default})


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
