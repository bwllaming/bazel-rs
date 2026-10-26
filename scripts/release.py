#!/usr/bin/env python3
"""Build, package and publish a bazel-rs binary release (docs/RELEASING.md).

    scripts/release.py [--version X.Y.Z] [--dry-run] [--notes FILE]

The release is built from the released commit in a fresh temporary `git
worktree` with its own CARGO_TARGET_DIR (never the caller's tree), smoke-tested
offline, packaged as a deterministic tarball and, unless --dry-run, published:
an annotated tag `v<version>` is pushed (only the tag; never a branch) and a
GitHub release with the archive and its checksums is created with `gh`, then
downloaded again and verified. Python 3.11+ standard library only.
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
import tomllib
from typing import Callable, Iterable, Sequence


ROOT = Path(__file__).resolve().parents[1]
REMOTE = "origin"
BRANCH = "main"
TARGET = "x86_64-unknown-linux-gnu"
PACKAGE = "bazel-rs"
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


def archive_stem(version: str) -> str:
    return f"{PACKAGE}-{version}-{TARGET}"


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

    if head:
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

    if git("rev-parse", "--quiet", "--verify", f"refs/tags/{tag}").returncode == 0:
        problems.append(f"tag {tag} already exists locally (`git tag -d {tag}` if stale)")
    remote_tag = git("ls-remote", "--tags", REMOTE, f"refs/tags/{tag}")
    if remote_tag.returncode != 0:
        problems.append(f"cannot list tags of {REMOTE}: {remote_tag.stderr.strip()}")
    elif remote_tag.stdout.strip():
        problems.append(f"tag {tag} already exists on {REMOTE}")

    if not gh:
        problems.append("gh (GitHub CLI) not found: pass --gh PATH or set GH")
    else:
        auth = run([gh, "auth", "status", "--hostname", "github.com"], check=False)
        if auth.returncode != 0:
            problems.append(f"gh is not authenticated: {auth.stderr.strip()} (run `gh auth login`)")
        else:
            view = run(
                [gh, "release", "view", tag, "--repo", gh_repo, "--json", "tagName"],
                check=False,
            )
            if view.returncode == 0:
                problems.append(f"GitHub release {tag} already exists in {gh_repo}")
            elif "not found" not in (view.stderr + view.stdout).lower():
                problems.append(
                    f"cannot check for an existing release {tag} in {gh_repo}: "
                    f"{view.stderr.strip()}"
                )
    return problems, head


# --------------------------------------------------------------------------
# Build


def build_env(base: dict, repo_dir: Path, src_dir: Path, target_dir: Path, epoch: int) -> dict:
    """The caller's environment minus build-shaping variables, plus ours.

    RUSTUP_HOME/CARGO_HOME are honoured; when unset, the checkout's
    .direnv/{rustup,cargo} are used as .envrc does. Paths of the temporary
    worktree and of CARGO_HOME are remapped so the binary does not embed them.
    """
    env = {k: v for k, v in base.items() if k not in SCRUBBED_ENV}
    for var, sub in (("RUSTUP_HOME", "rustup"), ("CARGO_HOME", "cargo")):
        if not env.get(var) and (repo_dir / ".direnv" / sub).is_dir():
            env[var] = str(repo_dir / ".direnv" / sub)
    cargo_home = env.get("CARGO_HOME") or str(Path(env.get("HOME", "~")).expanduser() / ".cargo")
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["SOURCE_DATE_EPOCH"] = str(epoch)
    env["RUSTFLAGS"] = (
        f"--remap-path-prefix={src_dir}=/bazel-rs --remap-path-prefix={cargo_home}=/cargo"
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


def smoke_test(run: Run, binary: Path, version: str, scratch: Path) -> str:
    """--version, version, help, and an offline query + build (twice).

    Uses the explicit-module profile (--experimental-builtin-modules=false), a
    one-module workspace and a dead proxy: nothing is downloaded.
    """
    for sub in ("home", "tmp", "ws"):
        (scratch / sub).mkdir(parents=True, exist_ok=True)
    ws = scratch / "ws"
    (ws / "MODULE.bazel").write_text(SMOKE_MODULE)
    (ws / "BUILD.bazel").write_text(SMOKE_BUILD)
    (ws / "input.txt").write_text(SMOKE_INPUT)
    env = smoke_env(scratch)

    def bazel(*args: str) -> Result:
        return run([str(binary), *args], cwd=ws, env=env, echo=True)

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
) -> str:
    out = [
        f"# Third-party licenses of bazel-rs {version}\n",
        f"Generated by `scripts/release.py` at commit `{commit}` from `Cargo.lock`",
        "(`cargo metadata --locked`) and the embedded Bazel tools bundle. bazel-rs",
        "itself is licensed under Apache-2.0 (`LICENSE`, `NOTICE`). Identical license",
        "texts are printed once and referenced afterwards.\n",
        "## Rust crates\n",
        f"{len(packages)} crates: the normal and build dependencies of bazel-rs for",
        f"`{TARGET}` (dev-dependencies excluded).\n",
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
    out.append("## License texts of the Rust crates\n")
    for p in packages:
        files = crate_files(p)
        out.append(f"### {p['name']} {p['version']}\n")
        if not files:
            out.append(f"No license file in the crate; license expression: {md_cell(p.get('license'))}.\n")
        for name, data in files:
            emit(f"`{p['name']} {p['version']}/{name}`", data)
    return "\n".join(out).rstrip("\n") + "\n"


def generate_third_party(run: Run, src: Path, env: dict, cargo: str, version: str, commit: str) -> str:
    metadata = json.loads(
        run(
            [cargo, "metadata", "--locked", "--format-version", "1", "--filter-platform", TARGET],
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
class NotesInfo:
    version: str
    commit: str
    commit_date: str
    compatibility: str
    glibc: str | None
    rustc: str
    archive: str
    archive_sha256: str
    gh_repo: str
    prerelease: bool
    previous_tag: str | None


def render_notes(info: NotesInfo, changelog: str, prepend: str | None = None) -> str:
    tag = tag_name(info.version)
    glibc = f"glibc ≥ {info.glibc}" if info.glibc else "glibc (version not determined)"
    stem = info.archive.removesuffix(".tar.gz")
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
        f"| Platform | Linux x86_64 (`{TARGET}`), {glibc} |",
        f"| Toolchain | {info.rustc} |",
        f"| Archive | `{info.archive}` |",
        f"| SHA-256 | `{info.archive_sha256}` |",
        "",
        "## Requirements",
        "",
        f"- Linux on x86_64 with {glibc} (e.g. Ubuntu 24.04 or newer).",
        "- No JVM and no Bazel installation: the binary embeds Bazel "
        f"{info.compatibility}'s `@bazel_tools`. Actions need the usual host tools "
        "(`/bin/sh`, coreutils; a C/C++ toolchain for cc rules).",
        "",
        "## Install",
        "",
        "```sh",
        f"gh release download {tag} --repo {info.gh_repo} --pattern '{info.archive}*'",
        f"sha256sum -c {info.archive}.sha256",
        f"tar xzf {info.archive}",
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
        "## Changes" + (f" since {info.previous_tag}" if info.previous_tag else ""),
        "",
        changelog.rstrip("\n"),
        "",
    ]
    return "\n".join(out)


# --------------------------------------------------------------------------
# Publishing


def recovery_commands(stage: str, tag: str, gh_repo: str, remote: str = REMOTE) -> list[str]:
    """Commands that undo a partly published release (printed, never run)."""
    commands = []
    if stage in ("released",):
        commands.append(f"gh release delete {tag} --repo {gh_repo} --yes")
    if stage in ("pushed", "released"):
        commands.append(f"git push {remote} :refs/tags/{tag}")
    commands.append(f"git tag -d {tag}")
    return commands


def publish(
    run: Run,
    *,
    repo_dir: Path,
    gh: str,
    gh_repo: str,
    version: str,
    commit: str,
    notes: Path,
    assets: list[Path],
    archive: Path,
    prerelease: bool,
    download_dir: Path,
) -> None:
    tag = tag_name(version)
    stage = "none"
    try:
        run(["git", "-C", repo_dir, "tag", "-a", tag, "-m", f"bazel-rs {version}", commit], echo=True)
        stage = "tagged"
        run(["git", "-C", repo_dir, "push", REMOTE, f"refs/tags/{tag}:refs/tags/{tag}"], echo=True)
        stage = "pushed"
        create = [
            gh, "release", "create", tag, "--repo", gh_repo, "--verify-tag",
            "--title", f"bazel-rs {version}", "--notes-file", str(notes),
            "--prerelease" if prerelease else "--latest",
            *map(str, assets),
        ]
        run(create, echo=True)
        stage = "released"
        view = json.loads(
            run([gh, "release", "view", tag, "--repo", gh_repo, "--json", "tagName,isPrerelease,assets,url"]).stdout
        )
        names = {a["name"] for a in view.get("assets", [])}
        missing = sorted({a.name for a in assets} - names)
        if view.get("tagName") != tag or missing or bool(view.get("isPrerelease")) != prerelease:
            raise ReleaseError(f"release {tag} does not look as created: {view} (missing {missing})")
        download_dir.mkdir(parents=True, exist_ok=True)
        run(
            [gh, "release", "download", tag, "--repo", gh_repo, "--pattern", archive.name, "--dir", str(download_dir)],
            echo=True,
        )
        got, want = sha256_file(download_dir / archive.name), sha256_file(archive)
        if got != want:
            raise ReleaseError(f"downloaded {archive.name} has sha256 {got}, uploaded {want}")
        print(f"published {view.get('url')} ({archive.name} sha256 {want}, verified by download)")
    except BaseException as error:
        if stage != "none":
            print(f"\nRELEASE FAILED after the tag was created (stage: {stage}): {error}", file=sys.stderr)
            print("Nothing was rolled back. To undo, run:", file=sys.stderr)
            for command in recovery_commands(stage, tag, gh_repo):
                print(f"    {command}", file=sys.stderr)
            if stage == "released":
                print(
                    f"or keep the release and re-upload: gh release upload {tag} --repo {gh_repo} --clobber "
                    + " ".join(shlex.quote(str(a)) for a in assets),
                    file=sys.stderr,
                )
        raise


# --------------------------------------------------------------------------
# Main


def default_gh() -> str | None:
    return os.environ.get("GH") or shutil.which("gh")


def git_out(run: Run, repo_dir: Path, *args: str) -> str:
    return run(["git", "-C", str(repo_dir), *args]).stdout.strip()


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", help="release version X.Y.Z (default: Cargo.toml's at HEAD)")
    parser.add_argument("--dry-run", action="store_true", help="build, test, package and write notes; no tag, push or release")
    parser.add_argument("--notes", type=Path, help="markdown prepended to the generated release notes")
    parser.add_argument("--replace-changelog", action="store_true", help="with --notes: drop the generated changelog")
    parser.add_argument("--latest", action="store_true", help="publish a 0.x/-pre version as a full release marked latest")
    parser.add_argument("--allow-dirty-for-test", action="store_true", help="skip the clean-tree check (the build uses the commit anyway)")
    parser.add_argument("--gh", default=default_gh(), help="GitHub CLI binary (default: $GH or gh on PATH)")
    parser.add_argument("--repo", help="GitHub owner/name (default: Cargo.toml repository)")
    parser.add_argument("--work-dir", type=Path, help="empty directory for the worktree, target and dist (default: a new temp dir)")
    parser.add_argument("--keep-build", action="store_true", help="keep the temporary worktree's CARGO_TARGET_DIR")
    args = parser.parse_args(argv)
    run: Run = run_command
    repo_dir = Path(git_out(run, ROOT, "rev-parse", "--show-toplevel"))
    if args.replace_changelog and not args.notes:
        parser.error("--replace-changelog needs --notes")

    cargo_toml_head = git_out(run, repo_dir, "show", "HEAD:Cargo.toml")
    version = args.version or cargo_toml_version(cargo_toml_head)
    parse_version(version)
    tag = tag_name(version)
    gh_repo = args.repo or repository_slug(cargo_toml_head)
    prerelease = is_prerelease(version, args.latest)
    prepend = args.notes.read_text() if args.notes else None
    print(f"release {tag} of {gh_repo} ({'prerelease' if prerelease else 'latest'}){' [dry run]' if args.dry_run else ''}")

    problems, commit = preflight(
        run, version, repo_dir=repo_dir, gh=args.gh, gh_repo=gh_repo, allow_dirty=args.allow_dirty_for_test
    )
    if problems:
        heading = "preflight would stop a real release:" if args.dry_run else "preflight failed:"
        print(heading, file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        if not args.dry_run:
            return 2
    if not commit:
        raise ReleaseError("no commit to release")
    if args.allow_dirty_for_test:
        print("note: --allow-dirty-for-test; uncommitted changes are NOT part of the release", file=sys.stderr)

    work = args.work_dir or Path(tempfile.mkdtemp(prefix=f"bazel-rs-release-{version}-"))
    work = work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    if any(work.iterdir()):
        raise ReleaseError(f"--work-dir {work} is not empty")
    src, target, dist, scratch = work / "src", work / "target", work / "dist", work / "smoke"
    dist.mkdir()
    epoch = int(git_out(run, repo_dir, "show", "-s", "--format=%ct", commit))
    commit_date = git_out(run, repo_dir, "show", "-s", "--format=%cI", commit)
    run(["git", "-C", repo_dir, "worktree", "add", "--detach", src, commit], echo=True)
    try:
        env = build_env(dict(os.environ), repo_dir, src, target, epoch)
        cargo = find_cargo(env)
        run([cargo, "build", "--locked", "--release"], cwd=src, env=env, capture=False)
        rustc = run(["rustc", "--version"], cwd=src, env={**env, "PATH": f"{Path(cargo).parent}:{env.get('PATH', '')}"}).stdout.strip()
        binary = target / "release" / PACKAGE
        if elf_machine(binary) != 62:
            raise ReleaseError(f"{binary} is not an x86-64 ELF executable")
        glibc = glibc_requirement(run, binary)
        third_party = generate_third_party(run, src, env, cargo, version, commit)

        stem = archive_stem(version)
        archive = dist / f"{stem}.tar.gz"
        files = [(PACKAGE, binary.read_bytes(), 0o755), (THIRD_PARTY, third_party.encode(), 0o644)]
        files += [(name, (src / name).read_bytes(), 0o644) for name in DOC_FILES]
        archive_sha = deterministic_tarball(archive, stem, files, epoch)
        checksum_files = write_checksums(dist, [archive])
        assets = [archive, *checksum_files]

        # Smoke-test the binary as shipped: extracted from the archive.
        unpacked = scratch / "unpacked"
        unpacked.mkdir(parents=True)
        with tarfile.open(archive) as tar:
            tar.extractall(unpacked, filter="data")
        shipped = unpacked / stem / PACKAGE
        compat = smoke_test(run, shipped, version, scratch)

        previous = previous_tag(run, repo_dir, commit, version)
        if args.replace_changelog:
            changelog = ""
        elif previous:
            log = run(["git", "-C", repo_dir, "log", "--no-merges", "--format=%H%x09%s", f"{previous}..{commit}"]).stdout
            changelog = changelog_from_log([tuple(l.split("\t", 1)) for l in log.splitlines() if "\t" in l])
        else:
            changelog = first_release_summary((src / "PLAN.md").read_text())
        info = NotesInfo(version, commit, commit_date, compat, glibc, rustc, archive.name, archive_sha, gh_repo, prerelease, previous)
        notes = work / "RELEASE_NOTES.md"
        notes.write_text(render_notes(info, changelog, prepend))

        print("\nartifacts:")
        for path in [*assets, notes]:
            print(f"  {path}")
        print(f"archive sha256 {archive_sha}; glibc >= {glibc}; compatibility target Bazel {compat}")
        create = [
            "gh", "release", "create", tag, "--repo", gh_repo, "--verify-tag", "--title", f"bazel-rs {version}",
            "--notes-file", str(notes), "--prerelease" if prerelease else "--latest", *map(str, assets),
        ]
        if args.dry_run:
            print("\nDRY RUN: a real release would now run:")
            print(f"  git tag -a {tag} -m 'bazel-rs {version}' {commit}")
            print(f"  git push {REMOTE} refs/tags/{tag}:refs/tags/{tag}")
            print("  " + shlex.join(create))
            print(f"  then verify with gh release view / gh release download ({archive.name} sha256)")
            return 1 if problems else 0
        # The build took a while: re-check the tag, the release and origin/main.
        problems, again = preflight(
            run, version, repo_dir=repo_dir, gh=args.gh, gh_repo=gh_repo, allow_dirty=args.allow_dirty_for_test
        )
        if problems or again != commit:
            for problem in problems or [f"HEAD moved from {commit} to {again} during the build"]:
                print(f"  - {problem}", file=sys.stderr)
            return 2
        publish(
            run, repo_dir=repo_dir, gh=args.gh, gh_repo=gh_repo, version=version, commit=commit,
            notes=notes, assets=assets, archive=archive, prerelease=prerelease, download_dir=work / "verify",
        )
        return 0
    finally:
        run(["git", "-C", repo_dir, "worktree", "remove", "--force", src], check=False, echo=True)
        if not args.keep_build:
            shutil.rmtree(target, ignore_errors=True)
        print(f"work directory: {work}" + ("" if args.keep_build else " (build directory removed; --keep-build keeps it)"))


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ReleaseError as error:
        print(f"release.py: {error}", file=sys.stderr)
        sys.exit(2)
