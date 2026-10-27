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
            ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"],
        )
        self.assertEqual(
            [t.triple for t in release.select_targets(["aarch64-unknown-linux-musl", "x86_64-unknown-linux-gnu"])],
            ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-musl"],
        )
        with self.assertRaises(release.ReleaseError):
            release.select_targets(["riscv64gc-unknown-linux-gnu"])
        arm = release.TARGETS_BY_TRIPLE["aarch64-unknown-linux-musl"]
        self.assertTrue(arm.static and not arm.default and arm.caveat)
        self.assertIn(("JEMALLOC_SYS_WITH_LG_PAGE", "16"), arm.env)
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
        sha256="d" * 64, requirement="statically linked", tested=release.SMOKE_QEMU, static=True,
        caveat=release.ARM64_CAVEAT,
    )

    def info(self, *assets):
        return release.NotesInfo(
            version="0.2.0", commit=HEAD, commit_date="2026-10-26T18:40:41+00:00", compatibility="9.3.0",
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
            "| Compatibility target | Bazel 9.3.0 |",
            "glibc ≥ 2.39",
            "gh release download v0.2.0 --repo o/r --pattern 'bazel-rs-0.2.0-x86_64-unknown-linux-gnu.tar.gz*'",
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
            f"- `aarch64-unknown-linux-musl`: any Linux arm64 distribution (kernel 3.2 or newer); statically linked, no glibc needed. **Not for general use:** {release.ARM64_CAVEAT}.",
            release.SMOKE_QEMU,
            "prefer the static one",
            # The install steps use the static x86_64 archive.
            "gh release download v0.2.0 --repo o/r --pattern 'bazel-rs-0.2.0-x86_64-unknown-linux-musl.tar.gz*'",
            "install -m 0755 bazel-rs-0.2.0-x86_64-unknown-linux-musl/bazel-rs",
        ):
            self.assertIn(needle, text)
        self.assertEqual(release.recommended_asset(self.info(self.ARM, self.GNU).assets).triple, "x86_64-unknown-linux-gnu")

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
