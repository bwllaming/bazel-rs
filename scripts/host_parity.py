#!/usr/bin/env python3
"""Live host parity: genuine Bazel 9.3.1 and bazel-rs side by side on this host
(docs/MACOS.md, "Genuine parity on macOS").

usage: host_parity.py fetch-genuine --dest DIR
       host_parity.py run --genuine BIN --bazel-rs BIN --work DIR
                          [--only CASE,...] [--report FILE] [--strict]

`fetch-genuine` downloads the official Bazel release binary of this host
(.bazelversion; darwin-arm64, linux-x86_64 or linux-arm64) from
releases.bazel.build into DIR, checks it against the release's .sha256 and the
pin below, and prints its path.

`run` gives each tool its own copy of every case's workspace (same basename,
so the convenience symlinks are named alike), its own state (genuine:
`--batch --output_user_root`, bazel-rs: `--cache-dir`), HOME and TMPDIR, runs
the case's steps in order under the same environment (`env -i` with PATH,
HOME, TMPDIR, LANG) and compares per step: the exit code, stdout, selected
stderr lines, the contents of selected output files and listings of selected
directory trees (runfiles). Before comparing, both tools' outputs are
normalized: their output base, execution root, output user root / cache
directory, workspace, HOME and TMPDIR become placeholders, sandbox
directory numbers `N`, durations and test-XML times `T`.

Every difference is printed as a unified diff and recorded in the report
(JSON). Differences listed in KNOWN (with the reason) are reported as known;
`--strict` exits 1 on any other difference. Python 3.9+ standard library only.
"""

from __future__ import annotations

import argparse
import difflib
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "tests" / "fixtures"

# The official 9.3.1 release binaries (releases.bazel.build/<v>/release/<name>.sha256).
GENUINE_SHA256 = {
    "bazel-9.3.1-darwin-arm64": "19c4958626e0e3a564a61c2e85eeaab97b550a30632b82341191aa61ed0ccc1b",
    "bazel-9.3.1-linux-x86_64": "7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c",
}


def host_platform_name() -> str:
    system, machine = platform.system(), platform.machine()
    if system == "Darwin" and machine in ("arm64", "aarch64"):
        return "darwin-arm64"
    if system == "Linux" and machine in ("x86_64", "amd64"):
        return "linux-x86_64"
    if system == "Linux" and machine in ("aarch64", "arm64"):
        return "linux-arm64"
    raise SystemExit(f"host_parity: unsupported host {system} {machine}")


def fetch_genuine(dest: Path) -> Path:
    version = (ROOT / ".bazelversion").read_text().strip()
    name = f"bazel-{version}-{host_platform_name()}"
    url = f"https://releases.bazel.build/{version}/release/{name}"
    dest.mkdir(parents=True, exist_ok=True)
    target = dest / name
    with urllib.request.urlopen(url + ".sha256", timeout=60) as response:
        published = response.read().decode().split()[0]
    pinned = GENUINE_SHA256.get(name)
    if pinned is not None and pinned != published:
        raise SystemExit(f"host_parity: {url}.sha256 is {published}, pinned {pinned}")
    if not target.exists() or sha256_file(target) != published:
        partial = dest / (name + ".partial")
        for attempt in range(3):
            try:
                with urllib.request.urlopen(url, timeout=300) as response, open(partial, "wb") as out:
                    shutil.copyfileobj(response, out)
                break
            except OSError:
                if attempt == 2:
                    raise
                time.sleep(5)
        got = sha256_file(partial)
        if got != published:
            raise SystemExit(f"host_parity: {name} has sha256 {got}, published {published}")
        partial.chmod(0o755)
        partial.replace(target)
    return target


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


# --------------------------------------------------------------------------
# Cases

# A step: `args` (the command and its options), and what to compare besides
# the exit code: "stdout" (default True), "stderr" (regexes: matching stderr
# lines), "files" (workspace-relative paths, through the convenience
# symlinks), "files_from_stdout" (compare the files stdout lists), "trees"
# (directory listings), "sort_stdout", "cquery" (stdout is cquery's label
# output: configuration checksums become `C` and labels their canonical
# spelling without `@@` and the module repositories' `+`, then the lines are
# sorted and deduplicated — bazel-rs's checksums are its own ConfigIds and
# its labels canonical by design, docs/CQUERY.md), "aquery" (stdout is
# aquery's text output: its action blocks sorted, bazel-rs orders them
# differently), "output_base_globs" (files below the output base, e.g. the
# darwin-sandbox profiles `--sandbox_debug` keeps; their indented lines
# sorted per block, genuine writes the writable directories from a hash set).
# `{WORKSPACE}` and `{HOME}` in arguments become the tool's paths. A case's
# "hosts" limits it to those hosts (host_platform_name()).
HP = "host_parity/workspace"
SANDBOX_PROBES = ["default", "block_network", "requires_network", "no_sandbox", "local"]

CASES = [
    {
        "name": "host_parity",
        "workspace": HP,
        "steps": [
            {"args": ["info", "output_path", "bazel-bin", "bazel-genfiles", "bazel-testlogs",
                      "execution_root", "output_base", "workspace"]},
            {"args": ["query", "//...", "--output=label_kind"]},
            {"args": ["query", "deps(//:env_test)", "--output=label"]},
            {"args": ["cquery", "//..."], "cquery": True},
            {"args": ["cquery", "deps(//:env_test)"], "cquery": True},
            {"args": ["cquery", "deps(//:exec_probe)"], "cquery": True},
            {"args": ["cquery", "//:selects", "--output=build"]},
            {"args": ["cquery", "//:probe + //:exec_probe + //:use_tool + //:tool", "--output=files"],
             "sort_stdout": True},
            {"args": ["cquery", "//:probe + //:make_vars", "--output=starlark",
                      "--starlark:expr=' '.join([str(target.label)] + [repr(build_options(target).get(k)) for k in "
                      "['//command_line_option:cpu', '//command_line_option:host_cpu', "
                      "'//command_line_option:platforms', '//command_line_option:host_platform', "
                      "'//command_line_option:apple_platform_type', '//command_line_option:compilation_mode', "
                      "'//command_line_option:action_env', '//command_line_option:shell_executable']])"],
             "sort_stdout": True},
            {"args": ["aquery", "//:make_vars + //:action_env + //:use_tool + //:exec_probe"], "aquery": True},
            {"args": ["build", "//...", "--build_tests_only=false"],
             "stderr": [r"^INFO: \d+ processes?: "],
             "files": ["bazel-bin/make_vars.txt", "bazel-bin/action_env.txt", "bazel-bin/host_info.txt",
                       "bazel-bin/selects.txt", "bazel-bin/use_tool.txt", "bazel-bin/probe.txt",
                       "bazel-bin/exec_probe.txt"],
             "trees": ["bazel-bin/tool.runfiles", "bazel-bin/env_test.runfiles"]},
            {"args": ["test", "//:env_test"],
             "stderr": [r"^INFO: \d+ processes?: ", r"^//:env_test "],
             "files": ["bazel-testlogs/env_test/test.log", "bazel-testlogs/env_test/test.xml"]},
            {"args": ["run", "//:tool"]},
        ],
    },
    {
        "name": "darwin_sandbox",
        "hosts": ["darwin-arm64"],
        "workspace": "darwin_sandbox/workspace",
        "steps": [
            {"args": ["build", "//...", "--sandbox_debug"],
             "stderr": [r"^INFO: \d+ processes?: "],
             "files": [f"bazel-bin/{n}.txt" for n in SANDBOX_PROBES],
             "output_base_globs": ["sandbox/*/*/sandbox.sb"]},
            {"args": ["build", "//...", "--sandbox_default_allow_network=false", "--action_env=PROBE_RUN=2"],
             "stderr": [r"^INFO: \d+ processes?: "],
             "files": [f"bazel-bin/{n}.txt" for n in SANDBOX_PROBES]},
            {"args": ["build", "//...", "--sandbox_writable_path=/Users/Shared", "--sandbox_block_path={WORKSPACE}",
                      "--action_env=PROBE_RUN=3"],
             "stderr": [r"^INFO: \d+ processes?: "],
             "files": [f"bazel-bin/{n}.txt" for n in SANDBOX_PROBES]},
        ],
    },
    {
        "name": "aquery_genrules",
        "workspace": "aquery/genrules/ws",
        "steps": [
            {"args": ["aquery", "//pkg:all"], "aquery": True},
            {"args": ["build", "//pkg:all_ok"], "files": ["bazel-bin/pkg/all_ok.txt"]},
        ],
    },
    {
        "name": "runfiles_symlinks",
        "workspace": "runfiles_symlinks/workspace",
        "steps": [
            {"args": ["test", "//:all", "--test_output=all"], "stdout": False,
             "stderr": [r"^//:\S+ +(PASSED|FAILED|NO STATUS)"]},
        ],
    },
    {
        "name": "test_args",
        "workspace": "test_args/workspace",
        "steps": [
            {"args": ["test", "//pkg:all", "--test_output=all"], "stdout": False,
             "stderr": [r"^//pkg:\S+ +(PASSED|FAILED|NO STATUS)"]},
        ],
    },
]

# Known differences, also on Linux (not host-specific): the diff lines
# (`+`/`-`) of an item matching `lines` are dropped; an item whose remaining
# diff is empty counts as known. `step` is the step's command prefix.
KNOWN = [
    {
        "case": "host_parity", "step": "cquery deps(", "item": "stdout",
        "lines": [r"^-@\S+//\S*:(toolchain_type|default_test_toolchain_type) \(C\)$"],
        "reason": "bazel-rs's cquery deps() omits the toolchain_type targets of toolchain resolution",
    },
]


def known_reason(case: str, args: list[str], item: str, diff: str) -> str | None:
    command = " ".join(args)
    rules = [k for k in KNOWN if k["case"] == case and command.startswith(k["step"]) and k["item"] == item]
    if not rules:
        return None
    patterns = [re.compile(p) for rule in rules for p in rule["lines"]]
    for line in diff.splitlines():
        if line.startswith(("---", "+++", "@@")) or not line.startswith(("+", "-")):
            continue
        if not any(p.search(line) for p in patterns):
            return None
    return "; ".join(rule["reason"] for rule in rules)


# --------------------------------------------------------------------------
# Running


class Tool:
    def __init__(self, name: str, binary: Path, state: Path):
        self.name = name
        self.binary = binary
        self.state = state

    def argv(self, args: list[str]) -> list[str]:
        if self.name == "genuine":
            startup = ["--batch", "--ignore_all_rc_files", f"--output_user_root={self.state / 'our'}"]
        else:
            startup = ["--ignore_all_rc_files", f"--cache-dir={self.state / 'cache'}"]
        return [str(self.binary), *startup, *args]


def tool_env(state: Path) -> dict[str, str]:
    home, tmp = state / "home", state / "tmp"
    home.mkdir(parents=True, exist_ok=True)
    tmp.mkdir(parents=True, exist_ok=True)
    env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(home), "TMPDIR": str(tmp), "LANG": "en_US.UTF-8"}
    if "USER" in os.environ:
        env["USER"] = os.environ["USER"]
    return env


def run_step(tool: Tool, ws: Path, env: dict[str, str], args: list[str], timeout: int) -> dict:
    started = time.monotonic()
    try:
        result = subprocess.run(tool.argv(args), cwd=ws, env=env, capture_output=True, timeout=timeout)
        code, out, err = result.returncode, result.stdout, result.stderr
    except subprocess.TimeoutExpired as e:
        code, out, err = "timeout", e.stdout or b"", e.stderr or b""
    return {
        "exit": code,
        "stdout": out.decode("utf-8", "replace"),
        "stderr": err.decode("utf-8", "replace"),
        "seconds": round(time.monotonic() - started, 2),
    }


def info_paths(tool: Tool, ws: Path, env: dict[str, str]) -> dict[str, str]:
    result = run_step(tool, ws, env, ["info", "output_base", "execution_root", "install_base"], 600)
    paths = {}
    for line in result["stdout"].splitlines():
        key, _, value = line.partition(": ")
        if value:
            paths[key] = value
    return paths


class Normalizer:
    def __init__(self, replacements: list[tuple[str, str]]):
        # Longest first, and each path with its realpath too (macOS: /var -> /private/var).
        pairs = []
        for path, label in replacements:
            if not path:
                continue
            pairs.append((path, label))
            real = os.path.realpath(path)
            if real != path:
                pairs.append((real, label))
        self.pairs = sorted(pairs, key=lambda p: -len(p[0]))

    def __call__(self, text: str) -> str:
        for path, label in self.pairs:
            text = text.replace(path, label)
        # A spawn's own execution root: genuine's sandbox directory, bazel-rs's
        # sandbox or test work directory (the strategy shows in the
        # `INFO: N processes` line).
        text = re.sub(r"<OUTPUT_BASE>/sandbox/[a-z-]+/\d+/execroot/_main", "<SPAWN_ROOT>", text)
        text = re.sub(r"<STATE>/(tests/)?work/[0-9a-f]+(-\d+)+/execroot", "<SPAWN_ROOT>", text)
        # bazel-rs's output store: the execution root's outputs are links into it.
        text = re.sub(r"<STATE>/actions/[0-9a-f]{64}/outputs/", "<EXECROOT>/", text)
        text = re.sub(r"/sandbox/([a-z-]+)/\d+/", r"/sandbox/\1/N/", text)
        text = re.sub(r"# Configuration: [0-9a-f]{64}", "# Configuration: <C>", text)
        text = re.sub(r'\b(time|timestamp|duration)="[^"]*"', r'\1="T"', text)
        text = re.sub(r"\b\d+(\.\d+)?s\b", "Ts", text)
        return text


def cquery_lines(text: str) -> str:
    lines = set()
    for line in text.splitlines():
        line = re.sub(r"\((?:[0-9a-f]{7,}|null)\)", lambda m: "(null)" if m.group(0) == "(null)" else "(C)", line)
        line = re.sub(r"@@([A-Za-z0-9_.~-]+)\+//", r"@\1//", line)
        line = line.replace("@@", "@")
        lines.add(line)
    return "\n".join(sorted(lines)) + "\n"


def aquery_blocks(text: str) -> str:
    blocks, current = [], []
    for line in text.splitlines(True):
        if line.startswith("action ") and current:
            blocks.append("".join(current))
            current = []
        current.append(line)
    if current:
        blocks.append("".join(current))
    return "".join(sorted(blocks))


def processes_line(line: str) -> str:
    """`INFO: N processes: ...`: the spawn counts per strategy only (cache
    hits and internal actions depend on the tools' caches)."""
    head, _, tail = line.partition(": ")
    _, _, counts = tail.partition(": ")
    kept = [part for part in counts.rstrip(".").split(", ")
            if not re.search(r"\b(internal|action cache hit|disk cache hit|remote cache hit)$", part)]
    return "INFO: processes: " + ", ".join(kept)


def tree_listing(root: Path, norm: Normalizer) -> str:
    if not root.exists():
        return "<missing>\n"
    lines = []
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        dirnames.sort()
        for name in sorted(dirnames + filenames):
            path = Path(dirpath) / name
            rel = path.relative_to(root)
            if path.is_symlink():
                target = os.readlink(path)
                lines.append(f"{rel} -> {norm(target)}")
            elif path.is_dir():
                lines.append(f"{rel}/")
            else:
                mode = "x" if os.access(path, os.X_OK) else "-"
                lines.append(f"{rel} [{mode}]")
    return "\n".join(sorted(lines)) + "\n"


def read_file(path: Path, norm: Normalizer) -> str:
    try:
        data = path.read_bytes()
    except FileNotFoundError:
        return "<missing>\n"
    except IsADirectoryError:
        return "<directory>\n"
    return norm(data.decode("utf-8", "replace"))


def sorted_blocks(text: str) -> str:
    out, run_ = [], []
    for line in text.splitlines(True):
        if line.startswith((" ", "\t")):
            run_.append(line)
            continue
        out.extend(sorted(run_))
        run_ = []
        out.append(line)
    out.extend(sorted(run_))
    return "".join(out)


def collect(tool: Tool, ws: Path, step: dict, result: dict, norm: Normalizer, output_base: Path) -> dict:
    items: dict[str, str] = {"exit": str(result["exit"])}
    if step.get("stdout", True):
        stdout = norm(result["stdout"])
        if step.get("sort_stdout"):
            stdout = "\n".join(sorted(stdout.splitlines())) + "\n"
        if step.get("cquery"):
            stdout = cquery_lines(stdout)
        if step.get("aquery"):
            stdout = aquery_blocks(stdout)
        items["stdout"] = stdout
    patterns = [re.compile(p) for p in step.get("stderr", [])]
    if patterns:
        lines = [norm(line) for line in result["stderr"].splitlines() if any(p.search(line) for p in patterns)]
        lines = [processes_line(line) if re.match(r"INFO: \d+ process", line) else line for line in lines]
        items["stderr"] = "\n".join(lines) + "\n"
    for rel in step.get("files", []):
        items[rel] = read_file(ws / rel, norm)
    if step.get("files_from_stdout"):
        for line in result["stdout"].splitlines():
            line = line.strip()
            if line:
                items[norm(line)] = read_file(ws / line, norm)
    for rel in step.get("trees", []):
        items[rel + "/"] = tree_listing(ws / rel, norm)
    for pattern in step.get("output_base_globs", []):
        texts = sorted(sorted_blocks(read_file(path, norm)) for path in output_base.glob(pattern))
        items["<OUTPUT_BASE>/" + pattern] = "".join(f"## file {i}\n{t}" for i, t in enumerate(texts)) or "<none>\n"
    return items


def run(args: argparse.Namespace) -> int:
    work = Path(args.work).resolve()
    if work.exists():
        # bazel-rs's repository trees are read-only.
        subprocess.run(["chmod", "-R", "u+w", str(work)], check=False)
        shutil.rmtree(work)
    only = set(args.only.split(",")) if args.only else None
    tools = [Tool("genuine", Path(args.genuine).resolve(), work / "genuine"),
             Tool("bazel-rs", Path(args.bazel_rs).resolve(), work / "bazel-rs")]
    report = {"host": host_platform_name(), "cases": []}
    unexpected = 0
    known = 0
    for case in CASES:
        if only and case["name"] not in only:
            continue
        if "hosts" in case and report["host"] not in case["hosts"] and not args.all_hosts:
            continue
        print(f"=== case {case['name']}", flush=True)
        per_tool = {}
        for tool in tools:
            ws = tool.state / case["name"] / "ws"
            if ws.exists():
                shutil.rmtree(ws)
            shutil.copytree(FIXTURES / case["workspace"], ws, symlinks=True)
            env = tool_env(tool.state)
            paths = info_paths(tool, ws, env)
            norm = Normalizer([
                (paths.get("execution_root", ""), "<EXECROOT>"),
                (paths.get("output_base", ""), "<OUTPUT_BASE>"),
                (paths.get("install_base", ""), "<INSTALL_BASE>"),
                (str(tool.state / "our"), "<STATE>"),
                (str(tool.state / "cache"), "<STATE>"),
                (str(ws), "<WORKSPACE>"),
                (env["HOME"], "<HOME>"),
                (env["TMPDIR"], "<TMP>"),
                (str(tool.binary), "<BAZEL>"),
            ])
            steps = []
            for step in case["steps"]:
                argv = [a.replace("{WORKSPACE}", str(ws)).replace("{HOME}", env["HOME"]) for a in step["args"]]
                result = run_step(tool, ws, env, argv, args.timeout)
                items = collect(tool, ws, step, result, norm, Path(paths.get("output_base", "/nonexistent")))
                steps.append({"result": result, "items": items})
                print(f"  {tool.name:8} exit={result['exit']} {result['seconds']:7.1f}s  {' '.join(step['args'])[:100]}",
                      flush=True)
            per_tool[tool.name] = steps
        case_report = {"name": case["name"], "steps": []}
        for index, step in enumerate(case["steps"]):
            g = per_tool["genuine"][index]
            r = per_tool["bazel-rs"][index]
            entry = {"args": step["args"], "items": {}, "genuine_stderr_tail": g["result"]["stderr"][-4000:],
                     "bazel_rs_stderr_tail": r["result"]["stderr"][-4000:]}
            for key in sorted(set(g["items"]) | set(r["items"])):
                gv = g["items"].get(key, "<absent>\n")
                rv = r["items"].get(key, "<absent>\n")
                if gv == rv:
                    entry["items"][key] = {"equal": True}
                    continue
                diff = "".join(difflib.unified_diff(
                    gv.splitlines(True), rv.splitlines(True), "genuine", "bazel-rs", n=2))
                reason = known_reason(case["name"], step["args"], key, diff)
                if reason:
                    known += 1
                else:
                    unexpected += 1
                entry["items"][key] = {"equal": False, "known": reason, "genuine": gv, "bazel_rs": rv, "diff": diff}
                tag = f"KNOWN ({reason})" if reason else "DIFF"
                print(f"--- {tag} {case['name']} step {index} `{' '.join(step['args'])[:80]}` {key}")
                print(diff[:args.diff_limit], end="" if diff.endswith("\n") else "\n")
            case_report["steps"].append(entry)
        report["cases"].append(case_report)
    report["unexpected"] = unexpected
    report["known"] = known
    if args.report:
        Path(args.report).write_text(json.dumps(report, indent=1, sort_keys=True) + "\n")
    print(f"host_parity: {unexpected} unexpected difference(s), {known} known")
    return 1 if (args.strict and unexpected) else 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    fetch = sub.add_parser("fetch-genuine")
    fetch.add_argument("--dest", required=True)
    runp = sub.add_parser("run")
    runp.add_argument("--genuine", required=True)
    runp.add_argument("--bazel-rs", required=True)
    runp.add_argument("--work", required=True)
    runp.add_argument("--only")
    runp.add_argument("--report")
    runp.add_argument("--strict", action="store_true")
    runp.add_argument("--all-hosts", action="store_true", help="also the cases of other hosts")
    runp.add_argument("--timeout", type=int, default=1800)
    runp.add_argument("--diff-limit", type=int, default=20000)
    args = parser.parse_args(argv)
    if args.command == "fetch-genuine":
        print(fetch_genuine(Path(args.dest)))
        return 0
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
