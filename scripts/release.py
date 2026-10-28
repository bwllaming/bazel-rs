#!/usr/bin/env python3
"""Build, package and publish a bazel-rs binary release (docs/RELEASING.md).

    scripts/release.py [--version X.Y.Z] [--notes FILE [--replace-changelog]] [--watch]
    scripts/release.py --dry-run [--target TRIPLE]...
    scripts/release.py --ci [--version X.Y.Z]        (.github/workflows/release.yml)

Releases are built and published by GitHub Actions. Locally (default) the
script checks that HEAD is a releasable origin/main, creates the annotated
tag `v<version>` (its message body carries --notes), pushes only the tag and
prints the workflow run it started (--watch waits for it). --dry-run instead
builds, smoke-tests and packages locally, in a fresh temporary `git worktree`
with its own CARGO_TARGET_DIR, without tagging. --ci (the workflow, on the
tag push) builds the checked-out tag in place once per target (default:
x86_64-unknown-linux-gnu, dynamically linked against the build host's glibc,
and x86_64-unknown-linux-musl, static, C code compiled with `zig cc`),
smoke-tests offline, packages deterministic tarballs, creates the GitHub
release of the existing tag with `gh` and verifies it by download; it never
creates or pushes tags. Python 3.11+ standard library only.
"""

from __future__ import annotations

import argparse
import dataclasses
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
from typing import Callable, Iterable, Sequence


ROOT = Path(__file__).resolve().parents[1]
REMOTE = "origin"
BRANCH = "main"
TARGET = "x86_64-unknown-linux-gnu"
PACKAGE = "bazel-rs"
EM_X86_64, EM_AARCH64 = 62, 183
ARM64_CAVEAT = (
    "experimental and opt-in: bazel-rs's embedded @bazel_tools and its repository host "
    "metadata (ctx.os) support only Linux x86_64, so most workspaces fail on arm64"
)


@dataclasses.dataclass(frozen=True)
class Target:
    """A release target: how it is built, checked and described."""

    triple: str
    arch: str  # as shown in the notes
    machine: int  # ELF e_machine
    # `zig cc -target` for the target's C code (jemalloc, ring, blake3,
    # src/musl_string.c); None: the host's cc and a plain `cargo build`.
    zig: str | None
    static: bool
    default: bool
    linker: str | None = None  # CARGO_TARGET_<TRIPLE>_LINKER
    env: tuple[tuple[str, str], ...] = ()
    caveat: str | None = None


TARGETS = (
    Target(TARGET, "x86_64", EM_X86_64, None, static=False, default=True),
    Target("x86_64-unknown-linux-musl", "x86_64", EM_X86_64, "x86_64-linux-musl", static=True, default=True),
    # jemalloc fixes the page size at build time: 64 KiB covers 4/16/64 KiB
    # kernels (README "Global allocator"). The host cc cannot link arm64, so
    # the toolchain's rust-lld links it (static musl needs no C driver).
    Target(
        "aarch64-unknown-linux-musl", "arm64", EM_AARCH64, "aarch64-linux-musl", static=True, default=False,
        linker="rust-lld", env=(("JEMALLOC_SYS_WITH_LG_PAGE", "16"),), caveat=ARM64_CAVEAT,
    ),
)
TARGETS_BY_TRIPLE = {t.triple: t for t in TARGETS}
# Statically linked C runtime of the musl targets (licence texts from the zig
# distribution that compiles their C code): musl libc and LLVM libunwind.
STATIC_RUNTIME_LICENSES = (
    ("musl libc (MIT)", "lib/libc/musl/COPYRIGHT"),
    ("LLVM libunwind (Apache-2.0 WITH LLVM-exception)", "lib/libunwind/LICENSE.TXT"),
)
# The aarch64 ELF magic/mask of a binfmt_misc qemu-aarch64 registration.
AARCH64_BINFMT_MAGIC = "7f454c460201010000000000000000000200b700"
# Copied from the released commit into the archive, next to the binary and
# the generated THIRD_PARTY_LICENSES.md.
DOC_FILES = ("LICENSE", "NOTICE", "README.md")
THIRD_PARTY = "THIRD_PARTY_LICENSES.md"
VERSION_RE = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)
VERSION_LINE_RE = re.compile(
    r"^bazel-rs (\S+) \(experimental; compatibility target Bazel (\S+)\)$"
)
GLIBC_RE = re.compile(r"\bGLIBC_(\d+(?:\.\d+)+)\b")
LICENSE_FILE_RE = re.compile(
    r"^(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE|UNLICENSE|AUTHORS)([-._].*)?$", re.I
)
# Variables that would make the build depend on the caller's environment
# rather than on the tagged commit and its rust-toolchain.toml.
SCRUBBED_ENV = (
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_BUILD_TARGET",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "RUSTUP_TOOLCHAIN",
    "CARGO_PROFILE_RELEASE_DEBUG",
    "CARGO_PROFILE_RELEASE_STRIP",
    "CARGO_PROFILE_RELEASE_LTO",
)
# Commit-subject prefix -> changelog group, in display order.
GROUPS = (
    ("feat", "Features"),
    ("parity", "Bazel parity"),
    ("perf", "Performance"),
    ("fix", "Fixes"),
    ("docs", "Documentation"),
    ("test", "Tests"),
    ("other", "Other changes"),
)
PREFIX_GROUP = {
    "feat": "feat",
    "parity": "parity",
    "perf": "perf",
    "fix": "fix",
    "harden": "fix",
    "docs": "docs",
    "doc": "docs",
    "test": "test",
    "tests": "test",
}
SMOKE_MODULE = 'module(name = "release_smoke")\n'
SMOKE_BUILD = """\
filegroup(name = "inputs", srcs = ["input.txt"])

genrule(
    name = "copy",
    srcs = [":inputs"],
    outs = ["copied.txt"],
    cmd = "cat $(location :inputs) > $@",
)

genrule(
    name = "upper",
    srcs = [":copied.txt"],
    outs = ["upper.txt"],
    cmd = "tr '[:lower:]' '[:upper:]' < $< > $@",
)
"""
SMOKE_INPUT = "hello from a bazel-rs release\n"
# Any accidental network access of the smoke test fails fast on this proxy.
DEAD_PROXY = "http://127.0.0.1:9"


class ReleaseError(Exception):
    """A release step failed; the message says what to do."""


# --------------------------------------------------------------------------
# Versions


def parse_version(text: str) -> tuple[int, int, int, str | None]:
    match = VERSION_RE.match(text.strip())
    if not match:
        raise ReleaseError(f"not a release version (X.Y.Z or X.Y.Z-pre): {text!r}")
    major, minor, patch, pre = match.groups()
    return int(major), int(minor), int(patch), pre


def tag_name(version: str) -> str:
    return f"v{version}"


def is_prerelease(version: str, latest: bool = False) -> bool:
    """0.x and X.Y.Z-pre versions are GitHub prereleases unless --latest."""
    if latest:
        return False
    major, _, _, pre = parse_version(version)
    return major == 0 or pre is not None


def archive_stem(version: str, triple: str = TARGET) -> str:
    return f"{PACKAGE}-{version}-{triple}"


def cargo_toml_version(text: str) -> str:
    return tomllib.loads(text)["package"]["version"]


def cargo_lock_version(text: str, name: str = PACKAGE) -> str:
    roots = [
        p
        for p in tomllib.loads(text).get("package", [])
        if p.get("name") == name and "source" not in p
    ]
    if len(roots) != 1:
        raise ReleaseError(f"Cargo.lock has {len(roots)} root entries for {name}")
    return roots[0]["version"]


def repository_slug(cargo_toml_text: str) -> str:
    url = tomllib.loads(cargo_toml_text)["package"].get("repository", "")
    match = re.match(r"^https://github\.com/([^/]+/[^/]+?)(?:\.git)?/?$", url)
    if not match:
        raise ReleaseError(f"Cargo.toml repository is not a GitHub URL: {url!r}")
    return match.group(1)


def version_key(version: str) -> tuple:
    major, minor, patch, pre = parse_version(version)
    # A prerelease sorts before its release.
    return (major, minor, patch, pre is None, pre or "")


# --------------------------------------------------------------------------
# Running commands (injectable for tests)


@dataclasses.dataclass
class Result:
    returncode: int
    stdout: str = ""
    stderr: str = ""


Run = Callable[..., Result]


def run_command(
    argv: Sequence[str],
    *,
    cwd: str | os.PathLike | None = None,
    env: dict | None = None,
    check: bool = True,
    capture: bool = True,
    echo: bool = False,
) -> Result:
    """subprocess.run with text output; `capture=False` streams to the terminal."""
    if echo or not capture:
        print("+ " + shlex.join(str(a) for a in argv), file=sys.stderr, flush=True)
    proc = subprocess.run(
        [str(a) for a in argv],
        cwd=cwd,
        env=env,
        text=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
    )
    result = Result(proc.returncode, proc.stdout or "", proc.stderr or "")
    if check and result.returncode != 0:
        raise ReleaseError(
            f"command failed (exit {result.returncode}): {shlex.join(map(str, argv))}\n"
            f"{result.stderr.strip()}"
        )
    return result


# --------------------------------------------------------------------------
# Preflight


def preflight(
    run: Run,
    version: str,
    *,
    repo_dir: Path,
    gh: str | None,
    gh_repo: str,
    allow_dirty: bool = False,
) -> tuple[list[str], str]:
    """Every reason the release must not proceed, and the commit to release.

    Read-only apart from `git fetch origin`. Never pushes anything.
    """
    problems: list[str] = []
    tag = tag_name(version)

    def git(*args: str) -> Result:
        return run(["git", "-C", str(repo_dir), *args], check=False)

    try:
        parse_version(version)
    except ReleaseError as error:
        problems.append(str(error))

    branch = git("symbolic-ref", "--quiet", "--short", "HEAD")
    if branch.returncode != 0:
        problems.append(f"HEAD is detached; check out `{BRANCH}`")
    elif branch.stdout.strip() != BRANCH:
        problems.append(
            f"on branch `{branch.stdout.strip()}`, not `{BRANCH}`; releases are cut from {BRANCH}"
        )

    status = git("status", "--porcelain", "--untracked-files=normal")
    dirty = [line for line in status.stdout.splitlines() if line.strip()]
    if status.returncode != 0:
        problems.append(f"git status failed: {status.stderr.strip()}")
    elif dirty and not allow_dirty:
        shown = "; ".join(dirty[:5]) + (" ..." if len(dirty) > 5 else "")
        problems.append(f"working tree is not clean ({len(dirty)} entries: {shown})")

    head = git("rev-parse", "--verify", "HEAD^{commit}").stdout.strip()
    if not head:
        problems.append("cannot resolve HEAD")

    fetch = git("fetch", "--quiet", REMOTE)
    if fetch.returncode != 0:
        problems.append(f"`git fetch {REMOTE}` failed: {fetch.stderr.strip()}")
    remote_ref = f"refs/remotes/{REMOTE}/{BRANCH}"
    remote_head = git("rev-parse", "--verify", "--quiet", remote_ref).stdout.strip()
    if not remote_head:
        problems.append(f"{REMOTE}/{BRANCH} does not exist; push {BRANCH} first")
    elif head and head != remote_head:
        counts = git("rev-list", "--left-right", "--count", f"HEAD...{remote_ref}")
        ahead, behind = (counts.stdout.split() + ["?", "?"])[:2]
        problems.append(
            f"HEAD {head[:12]} is not {REMOTE}/{BRANCH} {remote_head[:12]} "
            f"({ahead} ahead, {behind} behind): push {BRANCH} yourself "
            f"(`git push {REMOTE} {BRANCH}`) or pull, then re-run; "
            "release.py never pushes branches"
        )

    problems += version_problems(git, head, version)

    if git("rev-parse", "--quiet", "--verify", f"refs/tags/{tag}").returncode == 0:
        problems.append(f"tag {tag} already exists locally (`git tag -d {tag}` if stale)")
    remote_tag = git("ls-remote", "--tags", REMOTE, f"refs/tags/{tag}")
    if remote_tag.returncode != 0:
        problems.append(f"cannot list tags of {REMOTE}: {remote_tag.stderr.strip()}")
    elif remote_tag.stdout.strip():
        problems.append(f"tag {tag} already exists on {REMOTE}")

    problems += gh_problems(run, gh, gh_repo, tag)
    return problems, head


def version_problems(git: Callable[..., Result], head: str, version: str) -> list[str]:
    """Cargo.toml and Cargo.lock at `head` must say `version`."""
    problems: list[str] = []
    if not head:
        return problems
    for name, reader in (
        ("Cargo.toml", cargo_toml_version),
        ("Cargo.lock", cargo_lock_version),
    ):
        shown = git("show", f"{head}:{name}")
        try:
            found = reader(shown.stdout) if shown.returncode == 0 else None
        except (ReleaseError, tomllib.TOMLDecodeError, KeyError) as error:
            found = None
            problems.append(f"cannot read the version from {name}: {error}")
        if shown.returncode != 0:
            problems.append(f"cannot read {name} at HEAD")
        elif found is not None and found != version:
            problems.append(
                f"{name} says {found}, releasing {version}: "
                f"run scripts/bump_version.py {version} and commit"
            )
    return problems


def gh_problems(run: Run, gh: str | None, gh_repo: str, tag: str) -> list[str]:
    """gh must be authenticated and the repository must have no release `tag` yet."""
    if not gh:
        return ["gh (GitHub CLI) not found: pass --gh PATH or set GH"]
    auth = run([gh, "auth", "status", "--hostname", "github.com"], check=False)
    if auth.returncode != 0:
        return [f"gh is not authenticated: {auth.stderr.strip()} (run `gh auth login`)"]
    view = run([gh, "release", "view", tag, "--repo", gh_repo, "--json", "tagName"], check=False)
    if view.returncode == 0:
        return [f"GitHub release {tag} already exists in {gh_repo}"]
    if "not found" not in (view.stderr + view.stdout).lower():
        return [f"cannot check for an existing release {tag} in {gh_repo}: {view.stderr.strip()}"]
    return []


def ci_preflight(
    run: Run,
    version: str,
    *,
    repo_dir: Path,
    gh: str | None,
    gh_repo: str,
) -> tuple[list[str], str, str]:
    """--ci: every reason not to build and publish, the commit, the tag message body.

    The checkout is the tag's commit (detached HEAD, built in place), so
    there is no branch or pushed-ness check; instead the tag must exist on
    `origin` (it is fetched again, so an annotated tag is not replaced by a
    lightweight one) and point at HEAD. Never creates, moves or pushes a tag.
    """
    problems: list[str] = []
    tag = tag_name(version)

    def git(*args: str) -> Result:
        return run(["git", "-C", str(repo_dir), *args], check=False)

    try:
        parse_version(version)
    except ReleaseError as error:
        problems.append(str(error))

    fetch = git("fetch", "--quiet", "--force", "--no-tags", REMOTE, f"+refs/tags/{tag}:refs/tags/{tag}")
    if fetch.returncode != 0:
        problems.append(f"cannot fetch tag {tag} from {REMOTE}: {fetch.stderr.strip()}")
    head = git("rev-parse", "--verify", "HEAD^{commit}").stdout.strip()
    if not head:
        problems.append("cannot resolve HEAD")
    tagged = git("rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}").stdout.strip()
    if not tagged:
        problems.append(f"tag {tag} does not exist; release.py --ci never creates tags (push it with scripts/release.py)")
    elif head and tagged != head:
        problems.append(f"tag {tag} is commit {tagged[:12]}, HEAD is {head[:12]}: check out the tag")

    # Built in place: uncommitted changes would end up in the binary.
    status = git("status", "--porcelain", "--untracked-files=normal")
    dirty = [line for line in status.stdout.splitlines() if line.strip()]
    if status.returncode != 0:
        problems.append(f"git status failed: {status.stderr.strip()}")
    elif dirty:
        shown = "; ".join(dirty[:5]) + (" ..." if len(dirty) > 5 else "")
        problems.append(f"checkout is not clean ({len(dirty)} entries: {shown})")

    problems += version_problems(git, head, version)

    body = ""
    if tagged:
        if git("cat-file", "-t", f"refs/tags/{tag}").stdout.strip() == "tag":
            body = git("for-each-ref", "--format=%(contents:body)", f"refs/tags/{tag}").stdout
        else:
            print(f"note: {tag} is a lightweight tag: no extra release notes", file=sys.stderr)
    problems += gh_problems(run, gh, gh_repo, tag)
    return problems, head, body


# Tag annotation line: the notes in the tag replace the generated changelog
# (--replace-changelog). An HTML comment, so it is invisible if shown.
REPLACE_CHANGELOG_MARKER = "<!-- release.py: replace-changelog -->"


def tag_message(version: str, notes: str | None = None, replace_changelog: bool = False) -> str:
    """The annotated tag's message: subject `bazel-rs <version>`, body = --notes."""
    body = []
    if replace_changelog:
        body.append(REPLACE_CHANGELOG_MARKER)
    if notes and notes.strip():
        body.append(notes.strip("\n"))
    return f"bazel-rs {version}\n" + ("\n" + "\n".join(body) + "\n" if body else "")


def notes_from_tag_body(body: str) -> tuple[str | None, bool]:
    """(notes to prepend or None, replace the changelog) from a tag message body."""
    lines = body.splitlines()
    replace = any(line.strip() == REPLACE_CHANGELOG_MARKER for line in lines)
    text = "\n".join(line for line in lines if line.strip() != REPLACE_CHANGELOG_MARKER).strip("\n")
    return (text + "\n" if text.strip() else None), replace


def ci_run_url(env: dict) -> str | None:
    """The GitHub Actions run building this release (None outside Actions)."""
    server, repo, run_id = (env.get(k) for k in ("GITHUB_SERVER_URL", "GITHUB_REPOSITORY", "GITHUB_RUN_ID"))
    if not (server and repo and run_id):
        return None
    url = f"{server.rstrip('/')}/{repo}/actions/runs/{run_id}"
    attempt = env.get("GITHUB_RUN_ATTEMPT")
    return f"{url}/attempts/{attempt}" if attempt and attempt != "1" else url


# --------------------------------------------------------------------------
# Build


def build_env(base: dict, repo_dir: Path, src_dir: Path, target_dir: Path, epoch: int) -> dict:
    """The caller's environment minus build-shaping variables, plus ours.

    RUSTUP_HOME/CARGO_HOME are honoured; when unset, the checkout's
    .direnv/{rustup,cargo} are used as .envrc does. Paths of the temporary
    worktree, CARGO_HOME and RUSTUP_HOME are remapped so the binary does not
    embed them (the build does not depend on where it ran).
    """
    env = {k: v for k, v in base.items() if k not in SCRUBBED_ENV}
    for var, sub in (("RUSTUP_HOME", "rustup"), ("CARGO_HOME", "cargo")):
        if not env.get(var) and (repo_dir / ".direnv" / sub).is_dir():
            env[var] = str(repo_dir / ".direnv" / sub)
    home = Path(env.get("HOME", "~")).expanduser()
    cargo_home = env.get("CARGO_HOME") or str(home / ".cargo")
    rustup_home = env.get("RUSTUP_HOME") or str(home / ".rustup")
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["SOURCE_DATE_EPOCH"] = str(epoch)
    # With rust-src installed, std's panic locations name the local sysroot.
    env["RUSTFLAGS"] = " ".join(
        f"--remap-path-prefix={path}={alias}"
        for path, alias in ((src_dir, "/bazel-rs"), (cargo_home, "/cargo"), (rustup_home, "/rustup"))
    )
    return env


def find_cargo(env: dict) -> str:
    cargo_home = env.get("CARGO_HOME")
    if cargo_home and os.access(Path(cargo_home) / "bin" / "cargo", os.X_OK):
        return str(Path(cargo_home) / "bin" / "cargo")
    found = shutil.which("cargo", path=env.get("PATH"))
    if not found:
        raise ReleaseError("cargo not found: run ./scripts/setup-rust.sh or put the rustup proxy on PATH")
    return found


def elf_machine(path: Path) -> int | None:
    with open(path, "rb") as f:
        header = f.read(20)
    if len(header) < 20 or header[:4] != b"\x7fELF":
        return None
    return int.from_bytes(header[18:20], "little" if header[5] == 1 else "big")


def max_glibc(symbol_text: str) -> str | None:
    versions = {tuple(int(p) for p in v.split(".")) for v in GLIBC_RE.findall(symbol_text)}
    return ".".join(map(str, max(versions))) if versions else None


def glibc_requirement(run: Run, binary: Path) -> str | None:
    for argv in (["objdump", "-T", str(binary)], ["readelf", "--wide", "-V", str(binary)]):
        if shutil.which(argv[0]):
            result = run(argv, check=False)
            if result.returncode == 0:
                return max_glibc(result.stdout)
    return None


def env_triple(triple: str) -> str:
    """`x86_64-unknown-linux-musl` -> `X86_64_UNKNOWN_LINUX_MUSL` (cargo's env spelling)."""
    return triple.upper().replace("-", "_")


def zig_wrapper(zig: str, zig_target: str) -> str:
    """A `cc` for one target: `zig cc -target <zig_target>`.

    Drops the `--target=<rust triple>` the cc crate adds for clang-like
    compilers (zig wants its own spelling) and turns sanitizers off (zig cc
    enables UBSan for unoptimized C, whose runtime is not linked).
    """
    return (
        "#!/bin/sh\n"
        "# Generated by scripts/release.py.\n"
        "for a do\n"
        "  shift\n"
        '  case "$a" in --target=*|-target=*) ;; *) set -- "$@" "$a" ;; esac\n'
        "done\n"
        f'exec {shlex.quote(zig)} cc -target {zig_target} -fno-sanitize=all "$@"\n'
    )


def zig_tool(zig: str, tool: str) -> str:
    return f'#!/bin/sh\nexec {shlex.quote(zig)} {tool} "$@"\n'


def find_zig(explicit: str | None, repo_dir: Path, env: dict) -> str | None:
    """--zig, else $ZIG, else .direnv/zig/zig (scripts/setup-zig.sh), else zig on PATH."""
    for candidate in (explicit, env.get("ZIG"), str(repo_dir / ".direnv" / "zig" / "zig")):
        if candidate and os.access(candidate, os.X_OK):
            return str(Path(candidate).resolve())
    return shutil.which("zig", path=env.get("PATH"))


def cross_env(target: Target, zig: str, tools_dir: Path, zig_cache: Path) -> dict:
    """Environment of `cargo build --target <triple>` for a zig-compiled target.

    Writes the wrapper scripts into `tools_dir`.
    """
    tools_dir.mkdir(parents=True, exist_ok=True)
    scripts = {
        f"cc-{target.triple}": zig_wrapper(zig, target.zig),
        "zig-ar": zig_tool(zig, "ar"),
        "zig-ranlib": zig_tool(zig, "ranlib"),
    }
    for name, text in scripts.items():
        (tools_dir / name).write_text(text)
        (tools_dir / name).chmod(0o755)
    key = target.triple.replace("-", "_")
    env = {
        f"CC_{key}": str(tools_dir / f"cc-{target.triple}"),
        f"AR_{key}": str(tools_dir / "zig-ar"),
        f"RANLIB_{key}": str(tools_dir / "zig-ranlib"),
        "ZIG_GLOBAL_CACHE_DIR": str(zig_cache),
        "ZIG_LOCAL_CACHE_DIR": str(zig_cache),
        **dict(target.env),
    }
    if target.linker:
        env[f"CARGO_TARGET_{env_triple(target.triple)}_LINKER"] = target.linker
    return env


def elf_linkage(path: Path) -> tuple[bool, int]:
    """(has a PT_INTERP program interpreter, number of DT_NEEDED entries) of an ELF64 LE file."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        raise ReleaseError(f"{path} is not a little-endian ELF64 file")
    phoff = int.from_bytes(data[32:40], "little")
    phentsize = int.from_bytes(data[54:56], "little")
    phnum = int.from_bytes(data[56:58], "little")
    interp, needed = False, 0
    for i in range(phnum):
        ph = data[phoff + i * phentsize : phoff + (i + 1) * phentsize]
        kind = int.from_bytes(ph[0:4], "little")
        offset, size = int.from_bytes(ph[8:16], "little"), int.from_bytes(ph[32:40], "little")
        if kind == 3:  # PT_INTERP
            interp = True
        elif kind == 2:  # PT_DYNAMIC: (d_tag, d_val) pairs up to DT_NULL
            for at in range(offset, offset + size, 16):
                tag = int.from_bytes(data[at : at + 8], "little")
                if tag == 0:
                    break
                needed += tag == 1  # DT_NEEDED
    return interp, needed


def check_linkage(run: Run, target: Target, binary: Path) -> str:
    """The requirement line of the notes; fails unless static targets are static."""
    if elf_machine(binary) != target.machine:
        raise ReleaseError(f"{binary} is not an ELF executable for {target.arch} (e_machine {target.machine})")
    interp, needed = elf_linkage(binary)
    if target.static:
        if interp or needed:
            raise ReleaseError(
                f"{binary} for {target.triple} is not static: program interpreter {interp}, {needed} DT_NEEDED"
            )
        return "statically linked (musl libc, jemalloc): no glibc or other shared library needed"
    glibc = glibc_requirement(run, binary)
    return f"glibc ≥ {glibc}" if glibc else "glibc (version not determined)"


def binfmt_runs(machine: int, root: Path = Path("/proc/sys/fs/binfmt_misc")) -> bool:
    """Whether an enabled binfmt_misc entry runs aarch64 ELF files (e.g. qemu-user-static)."""
    if machine != EM_AARCH64 or not root.is_dir():
        return False
    for entry in root.iterdir():
        try:
            text = entry.read_text()
        except OSError:
            continue
        if text.startswith("enabled") and AARCH64_BINFMT_MAGIC in text.replace(" ", ""):
            return True
    return False


def host_machine() -> int | None:
    return {"x86_64": EM_X86_64, "aarch64": EM_AARCH64}.get(os.uname().machine)


# --------------------------------------------------------------------------
# Smoke test


def smoke_env(scratch: Path) -> dict:
    return {
        "PATH": "/usr/bin:/bin",
        "HOME": str(scratch / "home"),
        "TMPDIR": str(scratch / "tmp"),
        "LANG": "C.UTF-8",
        "BAZEL_RS_MAX_MEMORY_MB": "2048",
        "BAZEL_RS_PROGRESS": "0",
        # `label<TAB>path` result lines on stdout (off by default since 0.1.0).
        "BAZEL_RS_RESULT_LINES": "1",
        "http_proxy": DEAD_PROXY,
        "https_proxy": DEAD_PROXY,
        "HTTP_PROXY": DEAD_PROXY,
        "HTTPS_PROXY": DEAD_PROXY,
        "ALL_PROXY": DEAD_PROXY,
        "no_proxy": "",
        "NO_PROXY": "",
    }


def check_version_output(text: str, version: str) -> str:
    """The compatibility target named by `bazel-rs --version`."""
    match = VERSION_LINE_RE.match(text.strip())
    if not match:
        raise ReleaseError(f"unexpected `bazel-rs --version` output: {text!r}")
    if match.group(1) != version:
        raise ReleaseError(f"the binary says version {match.group(1)}, releasing {version}")
    return match.group(2)


def smoke_test(
    run: Run, binary: Path, version: str, scratch: Path, prefix: Sequence[str] = (), execute: bool = True
) -> str:
    """--version, version, help, and an offline query + build (twice).

    Uses the explicit-module profile (--experimental-builtin-modules=false), a
    one-module workspace and a dead proxy: nothing is downloaded. `prefix`
    runs the binary through an emulator (qemu-aarch64); `execute=False` stops
    after the query: actions are started through the binary's own
    /proc/self/exe, which only a binfmt_misc registration can run under QEMU.
    """
    for sub in ("home", "tmp", "ws"):
        (scratch / sub).mkdir(parents=True, exist_ok=True)
    ws = scratch / "ws"
    (ws / "MODULE.bazel").write_text(SMOKE_MODULE)
    (ws / "BUILD.bazel").write_text(SMOKE_BUILD)
    (ws / "input.txt").write_text(SMOKE_INPUT)
    env = smoke_env(scratch)

    def bazel(*args: str) -> Result:
        return run([*prefix, str(binary), *args], cwd=ws, env=env, echo=True)

    first = bazel("--version").stdout
    compat = check_version_output(first, version)
    if bazel("version").stdout != first:
        raise ReleaseError("`bazel-rs version` differs from `bazel-rs --version`")
    if "build" not in bazel("help").stdout:
        raise ReleaseError("`bazel-rs help` does not mention `build`")
    common = ["--ignore_all_rc_files", f"--cache-dir={scratch / 'cache'}"]
    profile = "--experimental-builtin-modules=false"
    query = bazel(*common, "query", profile, "//:all").stdout.split()
    if query != ["//:copy", "//:inputs", "//:upper"]:
        raise ReleaseError(f"unexpected query output: {query}")
    if not execute:
        return compat
    outputs = []
    for _ in range(2):  # the second build is all cache hits
        stdout = bazel(*common, "build", profile, "//:upper").stdout
        lines = [l for l in stdout.splitlines() if l.startswith("//:upper\t")]
        if len(lines) != 1:
            raise ReleaseError(f"unexpected build output: {stdout!r}")
        output = Path(lines[0].split("\t", 1)[1])
        if output.read_text() != SMOKE_INPUT.upper():
            raise ReleaseError(f"wrong build output in {output}")
        outputs.append(output)
    return compat


# --------------------------------------------------------------------------
# Third-party licenses


def runtime_packages(metadata: dict) -> list[dict]:
    """Packages reachable from the root through normal and build edges."""
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    root = metadata["resolve"]["root"]
    seen, stack = set(), [root]
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        for dep in nodes[current].get("deps", []):
            kinds = [k.get("kind") for k in dep.get("dep_kinds", [])] or [None]
            if any(kind != "dev" for kind in kinds):
                stack.append(dep["pkg"])
    seen.discard(root)
    return sorted((packages[i] for i in seen), key=lambda p: (p["name"], version_sort(p["version"])))


def version_sort(version: str) -> tuple:
    try:
        return version_key(version)
    except ReleaseError:
        return (0, 0, 0, False, version)


def license_files(crate_dir: Path) -> list[tuple[str, bytes]]:
    """License-like files at the crate root and one directory down (bundled C sources)."""
    found = []
    if not crate_dir.is_dir():
        return found
    for entry in sorted(crate_dir.iterdir()):
        if entry.is_file() and LICENSE_FILE_RE.match(entry.name):
            found.append((entry.name, entry.read_bytes()))
        elif entry.is_dir() and not entry.name.startswith(".") and entry.name not in ("tests", "benches", "examples", "target"):
            for inner in sorted(entry.iterdir()):
                if inner.is_file() and LICENSE_FILE_RE.match(inner.name):
                    found.append((f"{entry.name}/{inner.name}", inner.read_bytes()))
    return found


def bundle_legal_files(archive: Path) -> list[tuple[str, bytes]]:
    """Legal files of the embedded Bazel tools bundle (JDK legal notices etc.)."""
    found = []
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar:
            if not member.isfile():
                continue
            name = member.name.removeprefix("./")
            base = name.rsplit("/", 1)[-1]
            if (
                "/legal/" in f"/{name}"
                or LICENSE_FILE_RE.match(base)
                or base in ("ASSEMBLY_EXCEPTION", "DISCLAIMER")
            ):
                found.append((name, tar.extractfile(member).read()))
    return sorted(found)


def fence(text: str) -> str:
    longest = max((len(m) for m in re.findall(r"`+", text)), default=0)
    return "`" * max(3, longest + 1)


def text_block(data: bytes) -> str:
    text = data.decode("utf-8", errors="replace").rstrip("\n")
    marks = fence(text)
    return f"{marks}text\n{text}\n{marks}\n"


def md_cell(text: str | None) -> str:
    return (text or "-").replace("|", "\\|").replace("\n", " ")


def third_party_markdown(
    version: str,
    commit: str,
    packages: list[dict],
    crate_files: Callable[[dict], list[tuple[str, bytes]]],
    bazel_license: bytes,
    bundle_files: list[tuple[str, bytes]],
    target: str = TARGET,
    runtime_files: Sequence[tuple[str, bytes]] = (),
) -> str:
    out = [
        f"# Third-party licenses of bazel-rs {version}\n",
        f"Generated by `scripts/release.py` at commit `{commit}` from `Cargo.lock`",
        "(`cargo metadata --locked`) and the embedded Bazel tools bundle. bazel-rs",
        "itself is licensed under Apache-2.0 (`LICENSE`, `NOTICE`). Identical license",
        "texts are printed once and referenced afterwards.\n",
        "## Rust crates\n",
        f"{len(packages)} crates: the normal and build dependencies of bazel-rs for",
        f"`{target}` (dev-dependencies excluded).\n",
        "| Crate | Version | License | Repository |",
        "|---|---|---|---|",
    ]
    for p in packages:
        license = p.get("license") or (f"see {p['license_file']}" if p.get("license_file") else None)
        source = p.get("source") or ""
        name = p["name"] if source.startswith("registry+") else f"{p['name']} (vendored)"
        out.append(
            f"| {md_cell(name)} | {md_cell(p['version'])} | {md_cell(license)} | {md_cell(p.get('repository'))} |"
        )
    out.append("")
    printed: dict[str, str] = {}

    def emit(title: str, data: bytes) -> None:
        digest = hashlib.sha256(data).hexdigest()
        if digest in printed:
            out.append(f"{title}: same text as {printed[digest]}.\n")
            return
        printed[digest] = title
        out.append(f"{title}:\n")
        out.append(text_block(data))

    out.append("## Bazel 9.3.0 embedded tools (in the binary as @bazel_tools)\n")
    emit("`resources/bazel_tools/LICENSE`", bazel_license)
    for name, data in bundle_files:
        emit(f"`embedded_tools/{name}`", data)
    if runtime_files:
        out.append("## Statically linked C runtime\n")
        out.append(
            "The static binary also contains musl libc and LLVM libunwind (the Rust toolchain's\n"
            "self-contained musl runtime) and the GCC `crtbegin`/`crtend` objects (GCC Runtime\n"
            "Library Exception).\n"
        )
        for name, data in runtime_files:
            emit(name, data)
    out.append("## License texts of the Rust crates\n")
    for p in packages:
        files = crate_files(p)
        out.append(f"### {p['name']} {p['version']}\n")
        if not files:
            out.append(f"No license file in the crate; license expression: {md_cell(p.get('license'))}.\n")
        for name, data in files:
            emit(f"`{p['name']} {p['version']}/{name}`", data)
    return "\n".join(out).rstrip("\n") + "\n"


def static_runtime_licenses(zig: str) -> list[tuple[str, bytes]]:
    """The licence texts of STATIC_RUNTIME_LICENSES from the zig distribution."""
    lib_root = Path(zig).resolve().parent
    found = []
    for title, relative in STATIC_RUNTIME_LICENSES:
        path = lib_root / relative
        if not path.is_file():
            raise ReleaseError(f"{path} not found: cannot document {title} (a zig distribution with lib/ is needed)")
        found.append((title, path.read_bytes()))
    return found


def generate_third_party(
    run: Run,
    src: Path,
    env: dict,
    cargo: str,
    version: str,
    commit: str,
    target: str = TARGET,
    runtime_files: Sequence[tuple[str, bytes]] = (),
) -> str:
    metadata = json.loads(
        run(
            [cargo, "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
            cwd=src,
            env=env,
        ).stdout
    )
    return third_party_markdown(
        version,
        commit,
        runtime_packages(metadata),
        lambda p: license_files(Path(p["manifest_path"]).parent),
        (src / "resources/bazel_tools/LICENSE").read_bytes(),
        bundle_legal_files(src / "resources/bazel_tools/embedded_tools.tar.gz"),
        target,
        runtime_files,
    )


# --------------------------------------------------------------------------
# Packaging


def deterministic_tarball(out: Path, root: str, files: Iterable[tuple[str, bytes, int]], mtime: int) -> str:
    """Write `out` (.tar.gz) and return its sha256.

    Entries sorted by name below `root/`, mtime = `mtime`, uid/gid 0 (root),
    mode 0755/0644; ustar format; gzip header mtime 0 and no file name.
    """

    def info(name: str, mode: int, size: int = 0, kind: bytes = tarfile.REGTYPE) -> tarfile.TarInfo:
        ti = tarfile.TarInfo(name)
        ti.type, ti.mode, ti.size, ti.mtime = kind, mode, size, mtime
        ti.uid = ti.gid = 0
        ti.uname = ti.gname = "root"
        return ti

    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        tar.addfile(info(f"{root}/", 0o755, kind=tarfile.DIRTYPE))
        for name, data, mode in sorted(files):
            tar.addfile(info(f"{root}/{name}", 0o755 if mode & 0o111 else 0o644, len(data)), io.BytesIO(data))
    with open(out, "wb") as f:
        with gzip.GzipFile(filename="", mode="wb", fileobj=f, mtime=0, compresslevel=9) as gz:
            gz.write(raw.getvalue())
    return sha256_file(out)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_checksums(dist: Path, assets: list[Path]) -> list[Path]:
    """`<asset>.sha256` per asset and a SHA256SUMS (sha256sum -c format)."""
    written, lines = [], []
    for asset in sorted(assets):
        line = f"{sha256_file(asset)}  {asset.name}\n"
        side = dist / f"{asset.name}.sha256"
        side.write_text(line)
        written.append(side)
        lines.append(line)
    sums = dist / "SHA256SUMS"
    sums.write_text("".join(lines))
    return written + [sums]


# --------------------------------------------------------------------------
# Release notes


def classify(subject: str) -> str:
    match = re.match(r"^([A-Za-z]+)(?:\([^)]*\))?!?:", subject)
    return PREFIX_GROUP.get(match.group(1).lower(), "other") if match else "other"


def changelog_from_log(entries: list[tuple[str, str]]) -> str:
    """`git log --no-merges` (sha, subject) entries grouped by subject prefix."""
    grouped: dict[str, list[str]] = {key: [] for key, _ in GROUPS}
    for sha, subject in entries:
        if subject.startswith("release:"):
            continue
        grouped[classify(subject)].append(f"- {subject} ({sha[:9]})")
    out = []
    for key, title in GROUPS:
        if grouped[key]:
            out.append(f"### {title}\n")
            out.extend(grouped[key])
            out.append("")
    return "\n".join(out).rstrip("\n") + "\n" if out else "No changes.\n"


def first_release_summary(plan_text: str) -> str:
    """The "Status" section of PLAN.md, as the first release's changelog."""
    match = re.search(r"^## Status[^\n]*\n(.*?)(?=^## )", plan_text, re.M | re.S)
    if not match:
        return "First release.\n"
    body = re.sub(r"\s*\(see `artifacts/[^)]*\)", "", match.group(1)).strip()
    return f"First release. Status at this commit (from PLAN.md):\n\n{body}\n"


def previous_tag(run: Run, repo_dir: Path, commit: str, version: str) -> str | None:
    """The highest earlier `vX.Y.Z` tag reachable from `commit`."""
    tags = run(
        ["git", "-C", str(repo_dir), "tag", "--merged", commit, "--list", "v[0-9]*"],
        check=False,
    ).stdout.split()
    earlier = []
    for tag in tags:
        try:
            if version_key(tag[1:]) < version_key(version):
                earlier.append(tag)
        except ReleaseError:
            continue
    return max(earlier, key=lambda t: version_key(t[1:])) if earlier else None


@dataclasses.dataclass
class AssetInfo:
    """One archive of the release, as the notes describe it."""

    triple: str
    arch: str
    archive: str
    sha256: str
    requirement: str  # check_linkage()
    tested: str  # how it was smoke-tested
    static: bool = False
    caveat: str | None = None


@dataclasses.dataclass
class NotesInfo:
    version: str
    commit: str
    commit_date: str
    compatibility: str
    rustc: str
    assets: list[AssetInfo]
    gh_repo: str
    prerelease: bool
    previous_tag: str | None
    build_url: str | None = None  # the GitHub Actions run that built it


def recommended_asset(assets: list[AssetInfo]) -> AssetInfo:
    """The archive the install instructions use: static x86_64 first."""
    return min(assets, key=lambda a: (a.arch != "x86_64", not a.static, a.caveat is not None))


def render_notes(info: NotesInfo, changelog: str, prepend: str | None = None) -> str:
    tag = tag_name(info.version)
    best = recommended_asset(info.assets)
    stem = best.archive.removesuffix(".tar.gz")
    blob = f"https://github.com/{info.gh_repo}/blob/{tag}"
    out = []
    if prepend:
        out += [prepend.rstrip("\n"), ""]
    out += [
        f"# bazel-rs {info.version}",
        "",
        "Experimental native Rust implementation of a Bazel-compatible build system."
        + (" Pre-1.0: not full Bazel parity; expect behaviour changes between releases." if info.prerelease else ""),
        "",
        "| | |",
        "|---|---|",
        f"| Version | {info.version} (tag `{tag}`) |",
        f"| Commit | `{info.commit}` ({info.commit_date}) |",
        f"| Compatibility target | Bazel {info.compatibility} |",
        f"| Toolchain | {info.rustc} |",
        *([f"| Built by | [GitHub Actions run]({info.build_url}) |"] if info.build_url else []),
        "",
        "## Downloads",
        "",
        "| Archive | Platform | Linking | Smoke test | SHA-256 |",
        "|---|---|---|---|---|",
    ]
    for a in info.assets:
        out.append(f"| `{a.archive}` | Linux {a.arch} (`{a.triple}`) | {a.requirement} | {a.tested} | `{a.sha256}` |")
    out += ["", "## Requirements", ""]
    for a in info.assets:
        if a.static:
            line = f"- `{a.triple}`: any Linux {a.arch} distribution (kernel 3.2 or newer); statically linked, no glibc needed."
        else:
            line = f"- `{a.triple}`: Linux {a.arch} with {a.requirement} (e.g. Ubuntu 24.04 or newer)."
        if a.caveat:
            line += f" **Not for general use:** {a.caveat}."
        out.append(line)
    if len(info.assets) > 1:
        out.append(
            "- The archives contain the same bazel-rs, built from the same commit with the same "
            "allocator (jemalloc); prefer the static one unless you need glibc's (e.g. `LD_PRELOAD` tools)."
        )
    out += [
        "- No JVM and no Bazel installation: the binary embeds Bazel "
        f"{info.compatibility}'s `@bazel_tools`. Actions need the usual host tools "
        "(`/bin/sh`, coreutils; a C/C++ toolchain for cc rules).",
        "",
        "## Install",
        "",
        "```sh",
        f"gh release download {tag} --repo {info.gh_repo} --pattern '{best.archive}*'",
        f"sha256sum -c {best.archive}.sha256",
        f"tar xzf {best.archive}",
        f"install -m 0755 {stem}/bazel-rs ~/.local/bin/bazel-rs   # any directory on PATH",
        "bazel-rs --version",
        "```",
        "",
        f"Run it like Bazel from a Bazel {info.compatibility} workspace, e.g. `bazel-rs build //...`,",
        "`bazel-rs test //pkg/...`, `bazel-rs query ...`. It reads the workspace's `.bazelrc`",
        "files (`--ignore_all_rc_files` skips them) and keeps its cache in",
        "`<workspace>/bazel-rs-out` (startup option `--cache-dir=/absolute/path` overrides it;",
        "linked git worktrees share the main checkout's cache). It installs no `bazel`",
        "alias and does not replace Bazel or Bazelisk; to try it under the `bazel` name in",
        "one shell, use `alias bazel=bazel-rs`.",
        "",
        f"Documentation: [README.md]({blob}/README.md) (implemented subset, behaviour",
        f"changes, limitations), [PLAN.md]({blob}/PLAN.md) (status and remaining work).",
        "",
    ]
    if changelog.strip():  # --replace-changelog: the prepended notes say it all
        out += [
            "## Changes" + (f" since {info.previous_tag}" if info.previous_tag else ""),
            "",
            changelog.rstrip("\n"),
            "",
        ]
    return "\n".join(out)


# --------------------------------------------------------------------------
# Publishing (CI) and the tag flow (developer machine)

WORKFLOW = "release.yml"


def recovery_commands(stage: str, tag: str, gh_repo: str, remote: str = REMOTE, ci: bool = False) -> list[str]:
    """Commands that undo a partly done release (printed, never run).

    Locally (`ci=False`) the stages are "tagged" and "pushed"; in CI the tag
    is never touched, only a "released" GitHub release is undone.
    """
    commands = []
    if stage == "released":
        commands.append(f"gh release delete {tag} --repo {gh_repo} --yes")
    if ci:
        return commands
    if stage in ("pushed", "released"):
        commands.append(f"git push {remote} :refs/tags/{tag}")
    commands.append(f"git tag -d {tag}")
    return commands


def rerun_hint(tag: str, gh_repo: str) -> str:
    return f"gh workflow run {WORKFLOW} --repo {gh_repo} -f tag={tag}"


def release_create_argv(gh: str, tag: str, gh_repo: str, version: str, notes: Path, prerelease: bool, assets: Sequence[Path]) -> list[str]:
    return [
        gh, "release", "create", tag, "--repo", gh_repo, "--verify-tag",
        "--title", f"bazel-rs {version}", "--notes-file", str(notes),
        "--prerelease" if prerelease else "--latest",
        *map(str, assets),
    ]


def publish(
    run: Run,
    *,
    gh: str,
    gh_repo: str,
    version: str,
    notes: Path,
    assets: list[Path],
    archives: list[Path],
    prerelease: bool,
    download_dir: Path,
) -> None:
    """--ci: create the GitHub release of the existing tag, then verify it by download.

    Never creates or pushes the tag (`--verify-tag` fails if it is missing).
    """
    tag = tag_name(version)
    stage = "none"
    try:
        run(release_create_argv(gh, tag, gh_repo, version, notes, prerelease, assets), echo=True)
        stage = "released"
        view = json.loads(
            run([gh, "release", "view", tag, "--repo", gh_repo, "--json", "tagName,isPrerelease,assets,url"]).stdout
        )
        names = {a["name"] for a in view.get("assets", [])}
        missing = sorted({a.name for a in assets} - names)
        if view.get("tagName") != tag or missing or bool(view.get("isPrerelease")) != prerelease:
            raise ReleaseError(f"release {tag} does not look as created: {view} (missing {missing})")
        download_dir.mkdir(parents=True, exist_ok=True)
        for archive in archives:
            run(
                [gh, "release", "download", tag, "--repo", gh_repo, "--pattern", archive.name, "--dir", str(download_dir)],
                echo=True,
            )
            got, want = sha256_file(download_dir / archive.name), sha256_file(archive)
            if got != want:
                raise ReleaseError(f"downloaded {archive.name} has sha256 {got}, uploaded {want}")
            print(f"verified by download: {archive.name} sha256 {want}")
        print(f"published {view.get('url')}")
    except BaseException as error:
        print(f"\nRELEASE FAILED (stage: {stage}): {error}", file=sys.stderr)
        print(f"The tag {tag} is kept. Nothing was rolled back.", file=sys.stderr)
        commands = recovery_commands(stage, tag, gh_repo, ci=True)
        if commands:
            print("To undo the release, run:", file=sys.stderr)
            for command in commands:
                print(f"    {command}", file=sys.stderr)
            print(
                f"or keep it and re-upload: gh release upload {tag} --repo {gh_repo} --clobber "
                + " ".join(shlex.quote(a.name) for a in assets),
                file=sys.stderr,
            )
        print(f"Then re-run the workflow: {rerun_hint(tag, gh_repo)}", file=sys.stderr)
        raise


def find_run(
    run: Run,
    gh: str,
    gh_repo: str,
    tag: str,
    commit: str,
    *,
    attempts: int = 12,
    delay: float = 5.0,
    sleep: Callable[[float], None] = time.sleep,
) -> dict | None:
    """The release workflow run the tag push started (polls ~1 minute)."""
    argv = [
        gh, "run", "list", "--repo", gh_repo, "--workflow", WORKFLOW, "--event", "push",
        "--branch", tag, "--limit", "1", "--json", "databaseId,url,headSha,status",
    ]
    for attempt in range(attempts):
        if attempt:
            sleep(delay)
        result = run(argv, check=False)
        if result.returncode != 0:
            continue
        try:
            runs = json.loads(result.stdout or "[]")
        except json.JSONDecodeError:
            continue
        for entry in runs:
            if entry.get("headSha") == commit:
                return entry
    return None


def push_tag(
    run: Run,
    *,
    repo_dir: Path,
    gh: str,
    gh_repo: str,
    version: str,
    commit: str,
    message: str,
    watch: bool = False,
    sleep: Callable[[float], None] = time.sleep,
) -> int:
    """Local release: annotated tag (message = notes), push only the tag, show the CI run."""
    tag = tag_name(version)
    stage = "none"
    try:
        # verbatim: the default cleanup would drop markdown `#` headings as comments.
        run(["git", "-C", str(repo_dir), "tag", "-a", "--cleanup=verbatim", "-m", message, tag, commit], echo=True)
        stage = "tagged"
        run(["git", "-C", str(repo_dir), "push", REMOTE, f"refs/tags/{tag}:refs/tags/{tag}"], echo=True)
        stage = "pushed"
    except BaseException as error:
        if stage != "none":
            print(f"\nRELEASE FAILED after the tag was created (stage: {stage}): {error}", file=sys.stderr)
            print("Nothing was rolled back. To undo, run:", file=sys.stderr)
            for command in recovery_commands(stage, tag, gh_repo):
                print(f"    {command}", file=sys.stderr)
        raise
    print(f"pushed tag {tag} ({commit[:12]}); the {WORKFLOW} workflow builds and publishes the release")
    found = find_run(run, gh, gh_repo, tag, commit, sleep=sleep)
    if not found:
        print(
            f"no workflow run for {tag} found yet; see https://github.com/{gh_repo}/actions/workflows/{WORKFLOW} "
            f"or `gh run list --repo {gh_repo} --workflow {WORKFLOW} --branch {tag}`",
            file=sys.stderr,
        )
        return 1 if watch else 0
    run_id = str(found.get("databaseId"))
    print(f"workflow run: {found.get('url')}")
    if not watch:
        print(f"follow it with: gh run watch {run_id} --repo {gh_repo} --exit-status")
        return 0
    watched = run([gh, "run", "watch", run_id, "--repo", gh_repo, "--exit-status", "--interval", "30"], check=False, capture=False)
    if watched.returncode != 0:
        print(
            f"workflow run failed: {found.get('url')} (the tag {tag} stays). Retry: `gh run rerun {run_id} "
            f"--repo {gh_repo} --failed` or, after fixing the workflow on {BRANCH}, `{rerun_hint(tag, gh_repo)}`",
            file=sys.stderr,
        )
        return watched.returncode or 1
    view = run([gh, "release", "view", tag, "--repo", gh_repo, "--json", "url"], check=False)
    try:
        print(f"published {json.loads(view.stdout)['url']}")
    except (json.JSONDecodeError, KeyError, TypeError):
        print(f"workflow succeeded; `gh release view {tag} --repo {gh_repo}`")
    return 0


# --------------------------------------------------------------------------
# Main


def default_gh() -> str | None:
    return os.environ.get("GH") or shutil.which("gh")


def default_qemu_aarch64() -> str | None:
    return os.environ.get("QEMU_AARCH64") or shutil.which("qemu-aarch64-static") or shutil.which("qemu-aarch64")


def git_out(run: Run, repo_dir: Path, *args: str) -> str:
    return run(["git", "-C", str(repo_dir), *args]).stdout.strip()


def default_temp_root(env: dict) -> Path | None:
    """Where the default work directory goes: $TMPDIR; without it ~/opencodetmp
    when that exists (the preferred scratch root of the project's development
    machines); else the system default."""
    if env.get("TMPDIR"):
        return Path(env["TMPDIR"])
    preferred = Path(env.get("HOME") or "~").expanduser() / "opencodetmp"
    return preferred if preferred.is_dir() else None


def select_targets(names: Sequence[str] | None) -> list[Target]:
    """The --target selection in TARGETS order; default: the default targets."""
    if not names:
        return [t for t in TARGETS if t.default]
    unknown = sorted(set(names) - set(TARGETS_BY_TRIPLE))
    if unknown:
        raise ReleaseError(f"unknown --target {', '.join(unknown)}; known: {', '.join(TARGETS_BY_TRIPLE)}")
    return [t for t in TARGETS if t.triple in names]


SMOKE_FULL = "query and two builds"
SMOKE_QEMU = "under QEMU user emulation: version, help, query (no action execution)"
SMOKE_NONE = "**not smoke-tested**"


def smoke_plan(
    target: Target,
    qemu: str | None,
    allow_untested: bool,
    host: int | None,
    binfmt: Callable[[int], bool] = binfmt_runs,
) -> tuple[list[str] | None, bool, str]:
    """(argv prefix, or None to skip; whether actions execute; notes text) for one target."""
    if target.machine == host or binfmt(target.machine):
        return [], True, SMOKE_FULL
    if qemu:
        return [qemu], False, SMOKE_QEMU
    if allow_untested:
        return None, False, SMOKE_NONE
    raise ReleaseError(
        f"cannot run {target.triple} binaries here: pass --qemu-aarch64 PATH (qemu-user), "
        "register qemu-aarch64 with binfmt_misc, or --allow-untested to ship it labelled untested"
    )


@dataclasses.dataclass
class Built:
    notes: Path
    assets: list[Path]
    archives: list[Path]


def build_release(
    run: Run,
    *,
    repo_dir: Path,
    version: str,
    commit: str,
    gh_repo: str,
    prerelease: bool,
    targets: list[Target],
    plans: dict,
    zig: str | None,
    work: Path,
    in_place: bool,
    keep_build: bool,
    prepend: str | None,
    replace_changelog: bool,
    build_url: str | None = None,
) -> Built:
    """Build, package, smoke-test and write the notes into `work`.

    `in_place` (--ci): build the checkout itself (HEAD is the tag) into its
    ./target, which the workflow caches; otherwise a fresh detached worktree
    of `commit` under `work` with its own target directory.
    """
    work.mkdir(parents=True, exist_ok=True)
    if any(work.iterdir()):
        raise ReleaseError(f"--work-dir {work} is not empty")
    if in_place:
        src, target_dir = repo_dir, repo_dir / "target"
    else:
        src, target_dir = work / "src", work / "target"
    dist, scratch = work / "dist", work / "smoke"
    dist.mkdir()
    epoch = int(git_out(run, repo_dir, "show", "-s", "--format=%ct", commit))
    commit_date = git_out(run, repo_dir, "show", "-s", "--format=%cI", commit)
    if not in_place:
        run(["git", "-C", repo_dir, "worktree", "add", "--detach", src, commit], echo=True)
    try:
        env = build_env(dict(os.environ), repo_dir, src, target_dir, epoch)
        cargo = find_cargo(env)
        tool_path = f"{Path(cargo).parent}:{env.get('PATH', '')}"
        rustc = run(["rustc", "--version"], cwd=src, env={**env, "PATH": tool_path}).stdout.strip()
        built = []
        for t in targets:
            if t.zig:
                # rust-toolchain.toml lists the targets; an older install may lack them.
                rustup = shutil.which("rustup", path=tool_path)
                if rustup:
                    run([rustup, "target", "add", t.triple], cwd=src, env=env, capture=False)
                target_env = {**env, **cross_env(t, zig, work / "tools", work / "zig-cache")}
                run([cargo, "build", "--locked", "--release", "--target", t.triple], cwd=src, env=target_env, capture=False)
                binary = target_dir / t.triple / "release" / PACKAGE
            else:
                run([cargo, "build", "--locked", "--release"], cwd=src, env=env, capture=False)
                binary = target_dir / "release" / PACKAGE
            requirement = check_linkage(run, t, binary)
            runtime = static_runtime_licenses(zig) if t.static else ()
            third_party = generate_third_party(run, src, env, cargo, version, commit, t.triple, runtime)
            stem = archive_stem(version, t.triple)
            archive = dist / f"{stem}.tar.gz"
            files = [(PACKAGE, binary.read_bytes(), 0o755), (THIRD_PARTY, third_party.encode(), 0o644)]
            files += [(name, (src / name).read_bytes(), 0o644) for name in DOC_FILES]
            built.append((t, archive, deterministic_tarball(archive, stem, files, epoch), requirement))
        archives = [archive for _, archive, _, _ in built]
        assets = [*archives, *write_checksums(dist, archives)]

        # Smoke-test each binary as shipped: extracted from its archive.
        compat, infos = None, []
        for t, archive, sha, requirement in built:
            prefix, execute, tested = plans[t.triple]
            if prefix is not None:
                unpacked = scratch / t.triple / "unpacked"
                unpacked.mkdir(parents=True)
                with tarfile.open(archive) as tar:
                    tar.extractall(unpacked, filter="data")
                shipped = unpacked / archive_stem(version, t.triple) / PACKAGE
                found = smoke_test(run, shipped, version, scratch / t.triple, prefix, execute)
                if compat and found != compat:
                    raise ReleaseError(f"{t.triple} says compatibility target {found}, others {compat}")
                compat = found
            infos.append(AssetInfo(t.triple, t.arch, archive.name, sha, requirement, tested, t.static, t.caveat))
        if compat is None:
            raise ReleaseError("no archive could be smoke-tested")

        previous = previous_tag(run, repo_dir, commit, version)
        if replace_changelog:
            changelog = ""
        elif previous:
            log = run(["git", "-C", repo_dir, "log", "--no-merges", "--format=%H%x09%s", f"{previous}..{commit}"]).stdout
            changelog = changelog_from_log([tuple(l.split("\t", 1)) for l in log.splitlines() if "\t" in l])
        else:
            changelog = first_release_summary((src / "PLAN.md").read_text())
        info = NotesInfo(version, commit, commit_date, compat, rustc, infos, gh_repo, prerelease, previous, build_url)
        notes = work / "RELEASE_NOTES.md"
        notes.write_text(render_notes(info, changelog, prepend))

        print("\nartifacts:")
        for path in [*assets, notes]:
            print(f"  {path}")
        for a in infos:
            print(f"{a.archive}: sha256 {a.sha256}; {a.requirement}; smoke test: {a.tested}")
        print(f"compatibility target Bazel {compat}")
        return Built(notes, assets, archives)
    finally:
        if not in_place:
            run(["git", "-C", repo_dir, "worktree", "remove", "--force", src], check=False, echo=True)
            if not keep_build:
                shutil.rmtree(target_dir, ignore_errors=True)
        print(
            f"work directory: {work}"
            + ("" if in_place or keep_build else " (build directory removed; --keep-build keeps it)")
        )


def report_problems(problems: list[str], dry_run: bool) -> None:
    print("preflight would stop a real release:" if dry_run else "preflight failed:", file=sys.stderr)
    for problem in problems:
        print(f"  - {problem}", file=sys.stderr)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", help="release version X.Y.Z (default: Cargo.toml's at HEAD)")
    parser.add_argument("--dry-run", action="store_true", help="build, test, package and write notes; no tag, push or release")
    parser.add_argument("--ci", action="store_true", help="the release workflow: build the checked-out tag in place and publish it")
    parser.add_argument("--watch", action="store_true", help="after pushing the tag, wait for the workflow run (gh run watch)")
    parser.add_argument("--notes", type=Path, help="markdown prepended to the generated release notes (stored in the tag message)")
    parser.add_argument("--replace-changelog", action="store_true", help="with --notes: drop the generated changelog")
    parser.add_argument("--latest", action="store_true", help="publish a 0.x/-pre version as a full release marked latest")
    parser.add_argument("--allow-dirty-for-test", action="store_true", help="skip the clean-tree check (the build uses the commit anyway)")
    parser.add_argument("--gh", default=default_gh(), help="GitHub CLI binary (default: $GH or gh on PATH)")
    parser.add_argument("--repo", help="GitHub owner/name (default: Cargo.toml repository)")
    parser.add_argument(
        "--target", action="append", choices=list(TARGETS_BY_TRIPLE), metavar="TRIPLE",
        help="with --dry-run/--ci: build this target (repeatable; default: " + ", ".join(t.triple for t in TARGETS if t.default)
        + "; opt-in: " + ", ".join(t.triple for t in TARGETS if not t.default) + ")",
    )
    parser.add_argument("--zig", help="zig for the musl targets' C code (default: $ZIG, .direnv/zig/zig, zig on PATH)")
    parser.add_argument("--qemu-aarch64", default=default_qemu_aarch64(), help="qemu-user for the arm64 smoke test (default: $QEMU_AARCH64 or on PATH)")
    parser.add_argument("--allow-untested", action="store_true", help="ship a target this host cannot run, labelled untested")
    parser.add_argument("--work-dir", type=Path, help="empty directory for the worktree, target and dist (default: a new directory under $TMPDIR)")
    parser.add_argument("--keep-build", action="store_true", help="keep the temporary worktree's CARGO_TARGET_DIR")
    args = parser.parse_args(argv)
    sys.stdout.reconfigure(line_buffering=True)
    run: Run = run_command
    repo_dir = Path(git_out(run, ROOT, "rev-parse", "--show-toplevel"))
    if args.replace_changelog and not args.notes and not args.ci:
        parser.error("--replace-changelog needs --notes")
    if args.ci and (args.notes or args.replace_changelog):
        parser.error("--ci reads the extra notes from the tag message; --notes/--replace-changelog go to the local tag flow")
    if args.watch and (args.ci or args.dry_run):
        parser.error("--watch only applies to the tag flow (no --ci, no --dry-run)")
    building = args.ci or args.dry_run
    if not building and (args.target or args.allow_untested or args.work_dir or args.keep_build):
        parser.error("--target/--allow-untested/--work-dir/--keep-build need --dry-run or --ci: the workflow builds the release")

    cargo_toml_head = git_out(run, repo_dir, "show", "HEAD:Cargo.toml")
    version = args.version or cargo_toml_version(cargo_toml_head)
    parse_version(version)
    tag = tag_name(version)
    gh_repo = args.repo or repository_slug(cargo_toml_head)
    prerelease = is_prerelease(version, args.latest)
    mode = "ci" + (" dry run" if args.dry_run else "") if args.ci else ("dry run" if args.dry_run else "tag and push")
    print(f"release {tag} of {gh_repo} ({'prerelease' if prerelease else 'latest'}) [{mode}]")

    if not building:
        notes = args.notes.read_text() if args.notes else None
        problems, commit = preflight(
            run, version, repo_dir=repo_dir, gh=args.gh, gh_repo=gh_repo, allow_dirty=args.allow_dirty_for_test
        )
        if problems:
            report_problems(problems, False)
            return 2
        if args.latest:
            print("note: --latest is not passed to CI; the workflow publishes 0.x/-pre versions as prereleases", file=sys.stderr)
        return push_tag(
            run, repo_dir=repo_dir, gh=args.gh, gh_repo=gh_repo, version=version, commit=commit,
            message=tag_message(version, notes, args.replace_changelog), watch=args.watch,
        )

    targets = select_targets(args.target)
    print("targets: " + ", ".join(t.triple for t in targets))
    # Tools and smoke runners first: fail before the long build.
    zig = None
    if any(t.zig for t in targets):
        zig = find_zig(args.zig, repo_dir, dict(os.environ))
        if not zig:
            raise ReleaseError("zig not found (compiles the musl targets' C code): run scripts/setup-zig.sh, set ZIG or pass --zig")
    plans = {t.triple: smoke_plan(t, args.qemu_aarch64, args.allow_untested, host_machine()) for t in targets}

    if args.ci:
        problems, commit, body = ci_preflight(run, version, repo_dir=repo_dir, gh=args.gh, gh_repo=gh_repo)
        prepend, replace_changelog = notes_from_tag_body(body)
        build_url = ci_run_url(dict(os.environ))
    else:
        problems, commit = preflight(
            run, version, repo_dir=repo_dir, gh=args.gh, gh_repo=gh_repo, allow_dirty=args.allow_dirty_for_test
        )
        prepend = args.notes.read_text() if args.notes else None
        replace_changelog, build_url = args.replace_changelog, None
    if problems:
        report_problems(problems, args.dry_run)
        if not args.dry_run:
            return 2
    if not commit:
        raise ReleaseError("no commit to release")
    if args.allow_dirty_for_test and not args.ci:
        print("note: --allow-dirty-for-test; uncommitted changes are NOT part of the release", file=sys.stderr)
    if prepend or replace_changelog:
        print(f"extra notes: {len(prepend or '')} characters" + ("; replacing the changelog" if replace_changelog else ""))

    if args.work_dir:
        work = args.work_dir
    else:
        root = default_temp_root(dict(os.environ))
        work = Path(tempfile.mkdtemp(prefix=f"bazel-rs-release-{version}-", dir=root))
    built = build_release(
        run, repo_dir=repo_dir, version=version, commit=commit, gh_repo=gh_repo, prerelease=prerelease,
        targets=targets, plans=plans, zig=zig, work=work.resolve(), in_place=args.ci, keep_build=args.keep_build,
        prepend=prepend, replace_changelog=replace_changelog, build_url=build_url,
    )
    create = release_create_argv("gh", tag, gh_repo, version, built.notes, prerelease, built.assets)
    if args.dry_run:
        print("\nDRY RUN: a real release would now run:")
        if args.ci:
            print("  " + shlex.join(create))
        else:
            print(f"  git tag -a --cleanup=verbatim -m 'bazel-rs {version} [+ --notes]' {tag} {commit}")
            print(f"  git push {REMOTE} refs/tags/{tag}:refs/tags/{tag}")
            print(f"  then GitHub Actions ({WORKFLOW}) builds the tag again and runs: scripts/release.py --ci --version {version}")
            print("  which publishes with: " + shlex.join(create))
        print("  then verify with gh release view / gh release download (sha256 of every archive)")
        return 1 if problems else 0
    publish(
        run, gh=args.gh, gh_repo=gh_repo, version=version, notes=built.notes, assets=built.assets,
        archives=built.archives, prerelease=prerelease, download_dir=work.resolve() / "verify",
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ReleaseError as error:
        print(f"release.py: {error}", file=sys.stderr)
        sys.exit(2)
