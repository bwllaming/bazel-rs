#!/usr/bin/env python3
"""Open-source corpus: correctness and performance of bazel-rs against genuine
Bazel 9.3.1 on public bzlmod repositories (docs/OSS_CORPUS.md).

usage: oss_corpus.py clone --src DIR [--only NAME,...]
       oss_corpus.py run --src DIR --work DIR --genuine ELF --bazel-rs BIN
                         [--only NAME,...] [--phases P,...] [--jobs J]
                         [--timeout S] [--results FILE]
       oss_corpus.py table --results FILE

`clone` fetches every project of scripts/oss_corpus_repos.json at its pinned
commit (public GitHub only; `git fetch --depth 1 <url> <commit>`).

`run` gives each tool its OWN copy of the project (without .git) and its own
state (genuine: --output_user_root, a shared pre-extracted --install_base;
bazel-rs: --cache-dir), HOME and TMPDIR, then runs the phases in order:

  nobuild_cold  `build --nobuild --keep_going TARGETS` in fresh state
                (module resolution, every fetch, loading, analysis)
  nobuild_warm  the same again (genuine: --batch, so the JVM is cold but its
                on-disk state warm; bazel-rs: a fresh process as always)
  server        genuine only: the same twice in server mode, the second one
                measured (a warm server), then `shutdown`
  query         `query //...` (label sets compared)
  build         `build --keep_going TARGETS` (outputs compared: the file list
                of bazel-bin and the SHA-256 of a deterministic sample)
  test          `test --keep_going TESTS` (per-target statuses compared)

Genuine runs with --batch except in `server`. Both tools read the project's
own .bazelrc (--nohome_rc --nosystem_rc); every command runs under
`env -i PATH=/usr/bin:/bin HOME=<private> TMPDIR=<private>` at nice +10 with
the same --jobs. Measured per command: exit code, wall time, CPU time of the
client tree (wait4), peak RSS of the tool's own process (VmHWM polled; for
the genuine server, the server's VmHWM) and the stderr tail.

Results accumulate in a JSON file (one entry per project), so projects can be
run one at a time; `table` prints the Markdown summary.

Never point this at a private repository: genuine Bazel runs on every project.
"""
import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_CONFIG = os.path.join(HERE, "oss_corpus_repos.json")
TOOLS = ("genuine", "rs")
PHASES = ("nobuild_cold", "nobuild_warm", "server", "query", "build", "test")
SAMPLE_SIZE = 200
TEST_STATUS_RE = re.compile(
    r"^(?P<label>(?:@@?[\w.~+-]*)?//\S*)\s+(?:\(cached\)\s+)?"
    r"(?P<status>PASSED|FAILED TO BUILD|FAILED|TIMEOUT|FLAKY|NO STATUS|SKIPPED|INCOMPLETE)\b"
)
PARAMS_RE = re.compile(r"-\d+\.params$")
ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")

# ---------------------------------------------------------------- pure parts


def load_config(path):
    """Load and validate the project list."""
    with open(path, encoding="utf-8") as fh:
        cfg = json.load(fh)
    names = set()
    for p in cfg["projects"]:
        for key in ("name", "git", "commit"):
            if not p.get(key):
                raise ValueError(f"project without {key}: {p}")
        if not re.fullmatch(r"[0-9a-f]{40}", p["commit"]):
            raise ValueError(f"{p['name']}: commit must be a full sha1")
        if not p["git"].startswith("https://github.com/"):
            raise ValueError(f"{p['name']}: only public GitHub sources are allowed")
        if p["name"] in names:
            raise ValueError(f"duplicate project {p['name']}")
        names.add(p["name"])
        p.setdefault("subdir", "")
        p.setdefault("flags", [])
        p.setdefault("targets", ["//..."])
        p.setdefault("test_targets", p["targets"])
        p.setdefault("phases", list(PHASES))
        for ph in p["phases"]:
            if ph not in PHASES:
                raise ValueError(f"{p['name']}: unknown phase {ph}")
    return cfg


def select(projects, only):
    if not only:
        return projects
    wanted = [n for n in only.split(",") if n]
    known = {p["name"]: p for p in projects}
    missing = [n for n in wanted if n not in known]
    if missing:
        raise ValueError(f"unknown projects: {', '.join(missing)}")
    return [known[n] for n in wanted]


def tool_argv(tool, binary, state, command, flags, targets, batch=True):
    """The argv of one command. `state` is the tool's private state dir."""
    if tool == "genuine":
        startup = [
            f"--output_user_root={state}/root",
            f"--install_base={os.path.normpath(os.path.join(state, '..', '..', 'genuine-install'))}",
            "--nohome_rc",
            "--nosystem_rc",
            # The JVM expands rc files' `~` with user.home (getpwuid), not $HOME.
            f"--host_jvm_args=-Duser.home={state}/home",
        ]
        if batch:
            startup.append("--batch")
    else:
        startup = ["--nohome_rc", "--nosystem_rc", f"--cache-dir={state}/cache"]
    argv = [binary, *startup, command, *flags]
    if targets:
        argv += ["--", *targets]
    return argv


def parse_test_statuses(text):
    """{label: status} from the test summary lines of a `test` run."""
    out = {}
    for line in ANSI_RE.sub("", text).splitlines():
        m = TEST_STATUS_RE.match(line.strip())
        if m:
            out[m.group("label")] = m.group("status")
    return out


def parse_query_labels(text):
    return sorted({ln.strip() for ln in text.splitlines() if ln.strip().startswith(("//", "@"))})


def sample_paths(paths, size=SAMPLE_SIZE):
    """A deterministic, evenly spread sample of a sorted path list."""
    paths = sorted(paths)
    if len(paths) <= size:
        return paths
    step = len(paths) / size
    return [paths[int(i * step)] for i in range(size)]


def compare_sets(a, b, limit=10):
    a, b = set(a), set(b)
    return {
        "common": len(a & b),
        "only_genuine": len(a - b),
        "only_rs": len(b - a),
        "only_genuine_examples": sorted(a - b)[:limit],
        "only_rs_examples": sorted(b - a)[:limit],
    }


def compare_statuses(g, r, limit=10):
    diff = {k: (g.get(k), r.get(k)) for k in sorted(set(g) | set(r)) if g.get(k) != r.get(k)}
    return {
        "genuine": summarize_statuses(g),
        "rs": summarize_statuses(r),
        "differences": len(diff),
        "examples": dict(list(diff.items())[:limit]),
    }


def summarize_statuses(statuses):
    counts = {}
    for s in statuses.values():
        counts[s] = counts.get(s, 0) + 1
    return counts


def verdict(entry):
    """OK / DIFF / RS-FAIL / G-FAIL from one project's results."""
    tools = entry.get("tools", {})
    g, r = tools.get("genuine", {}), tools.get("rs", {})
    worst = "OK"
    for phase in PHASES:
        if phase == "server" or phase not in g or phase not in r:
            continue
        ge, re_ = g[phase].get("exit"), r[phase].get("exit")
        if ge != 0 and ge == re_:
            worst = max_verdict(worst, "BOTH-FAIL")
        elif ge == 0 and re_ != 0:
            return "RS-FAIL"
        elif ge != re_:
            worst = max_verdict(worst, "DIFF")
    cmp_ = entry.get("compare", {})
    if cmp_.get("query", {}).get("only_genuine") or cmp_.get("query", {}).get("only_rs"):
        worst = max_verdict(worst, "DIFF")
    out = cmp_.get("outputs", {})
    if out.get("only_genuine") or out.get("only_rs") or out.get("hash_mismatches"):
        worst = max_verdict(worst, "DIFF")
    if cmp_.get("tests", {}).get("differences"):
        worst = max_verdict(worst, "DIFF")
    return worst


_ORDER = ["OK", "BOTH-FAIL", "DIFF", "RS-FAIL"]


def max_verdict(a, b):
    return a if _ORDER.index(a) >= _ORDER.index(b) else b


def merge_result(old, new):
    """Merge a run's phases into a project's earlier results (same commit)."""
    if not old or old.get("commit") != new.get("commit"):
        return new
    merged = {"commit": new["commit"], "tools": {}, "compare": dict(old.get("compare", {}))}
    for tool in TOOLS:
        merged["tools"][tool] = dict(old.get("tools", {}).get(tool, {}))
        merged["tools"][tool].update(new.get("tools", {}).get(tool, {}))
    merged["compare"].update(new.get("compare", {}))
    return merged


def fmt_s(rec):
    if not rec or rec.get("wall") is None:
        return "—"
    mark = "" if rec.get("exit") == 0 else f" (exit {rec.get('exit')})"
    return f"{rec['wall']:.1f}{mark}"


def fmt_mb(rec):
    if not rec or not rec.get("rss_kb"):
        return "—"
    return f"{rec['rss_kb'] / 1024:.0f}"


def render_table(results):
    lines = [
        "| project | verdict | cold nobuild g / rs (s) | warm nobuild g-batch / g-server / rs (s) "
        "| build g / rs (s) | peak RSS cold g / rs (MB) | outputs (common, only g, only rs, hash diff) "
        "| tests g / rs |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for name in sorted(results):
        e = results[name]
        g = e.get("tools", {}).get("genuine", {})
        r = e.get("tools", {}).get("rs", {})
        out = e.get("compare", {}).get("outputs")
        outs = (
            f"{out['common']}, {out['only_genuine']}, {out['only_rs']}, {out.get('hash_mismatches', 0)}"
            if out
            else "—"
        )
        tests = e.get("compare", {}).get("tests")
        tst = (
            f"{fmt_counts(tests['genuine'])} / {fmt_counts(tests['rs'])}" if tests else "—"
        )
        lines.append(
            f"| {name} | {verdict(e)} | {fmt_s(g.get('nobuild_cold'))} / {fmt_s(r.get('nobuild_cold'))} "
            f"| {fmt_s(g.get('nobuild_warm'))} / {fmt_s(g.get('server'))} / {fmt_s(r.get('nobuild_warm'))} "
            f"| {fmt_s(g.get('build'))} / {fmt_s(r.get('build'))} "
            f"| {fmt_mb(g.get('nobuild_cold'))} / {fmt_mb(r.get('nobuild_cold'))} | {outs} | {tst} |"
        )
    return "\n".join(lines)


def fmt_counts(counts):
    return " ".join(f"{k}:{v}" for k, v in sorted(counts.items())) or "0"


# ---------------------------------------------------------------- execution


def require_abs(path, what):
    if not os.path.isabs(path):
        sys.exit(f"{what} must be an absolute path: {path}")
    return path


def proc_hwm_kb(pid):
    try:
        with open(f"/proc/{pid}/status", encoding="utf-8") as fh:
            for line in fh:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1])
    except OSError:
        return None
    return None


def run_cmd(argv, cwd, env, log_prefix, timeout):
    """Run one command; returns the measurement record."""
    out_path, err_path = log_prefix + ".out", log_prefix + ".err"
    t0 = time.monotonic()
    with open(out_path, "wb") as out, open(err_path, "wb") as err:
        proc = subprocess.Popen(
            ["nice", "-n", "10", *argv], cwd=cwd, env=env, stdout=out, stderr=err,
            start_new_session=True,
        )
        hwm = [0]
        done = threading.Event()

        def poll():
            # nice execs the tool in place, so proc.pid is the tool itself.
            while not done.is_set():
                v = proc_hwm_kb(proc.pid)
                if v:
                    hwm[0] = max(hwm[0], v)
                done.wait(0.05)

        th = threading.Thread(target=poll, daemon=True)
        th.start()
        timed_out = False
        try:
            _, status, ru = os.wait4(proc.pid, 0) if timeout is None else wait_timeout(proc, timeout)
        except TimeoutError:
            timed_out = True
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except OSError:
                pass
            _, status, ru = os.wait4(proc.pid, 0)
        proc.returncode = os.waitstatus_to_exitcode(status)
        done.set()
        th.join()
    wall = time.monotonic() - t0
    with open(err_path, "rb") as fh:
        tail = fh.read()[-4000:].decode("utf-8", "replace")
    return {
        "argv": argv,
        "exit": "timeout" if timed_out else proc.returncode,
        "wall": round(wall, 2),
        "cpu": round(ru.ru_utime + ru.ru_stime, 2),
        "rss_kb": hwm[0] or None,
        "max_child_rss_kb": ru.ru_maxrss,
        "stderr_tail": ANSI_RE.sub("", tail),
    }


def wait_timeout(proc, timeout):
    deadline = time.monotonic() + timeout
    while True:
        pid, status, ru = os.wait4(proc.pid, os.WNOHANG)
        if pid:
            return pid, status, ru
        if time.monotonic() > deadline:
            raise TimeoutError
        time.sleep(0.05)


def git(args, cwd):
    subprocess.run(["git", *args], cwd=cwd, check=True)


def clone(project, src):
    dest = os.path.join(src, project["name"])
    if os.path.isdir(os.path.join(dest, ".git")):
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=dest, capture_output=True, text=True).stdout.strip()
        if head == project["commit"] and not (
            project.get("submodules") and not os.listdir(dest)
        ):
            if project.get("submodules"):
                git(["submodule", "update", "-q", "--init", "--recursive", "--depth", "1"], dest)
            return dest
    os.makedirs(dest, exist_ok=True)
    git(["init", "-q"], dest)
    # An origin remote resolves relative submodule URLs.
    subprocess.run(["git", "remote", "add", "origin", project["git"]], cwd=dest, stderr=subprocess.DEVNULL)
    git(["fetch", "-q", "--depth", "1", "origin", project["commit"]], dest)
    git(["checkout", "-q", "--force", "FETCH_HEAD"], dest)
    if project.get("submodules"):
        git(["submodule", "update", "-q", "--init", "--recursive", "--depth", "1"], dest)
    return dest


def apply_edits(project, ws):
    for rel in project.get("delete", []):
        path = os.path.join(ws, rel)
        if os.path.isdir(path):
            shutil.rmtree(path)
        elif os.path.lexists(path):
            os.remove(path)
    for rep in project.get("replace", []):
        path = os.path.join(ws, rep["file"])
        with open(path, encoding="utf-8") as fh:
            text = fh.read()
        if rep["old"] not in text:
            raise ValueError(f"{project['name']}: {rep['file']} lacks {rep['old']!r}")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(text.replace(rep["old"], rep["new"]))
    for com in project.get("comment_out", []):
        path = os.path.join(ws, com["file"])
        with open(path, encoding="utf-8") as fh:
            text = fh.read()
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(comment_out(text, com["lines"]))


def comment_out(text, needles):
    """Comment out every line containing one of `needles`."""
    out = []
    for line in text.splitlines(keepends=True):
        if any(n in line for n in needles) and not line.lstrip().startswith("#"):
            line = "# oss_corpus workaround: " + line
        out.append(line)
    return "".join(out)


def rmtree_force(path):
    if os.path.lexists(path):
        subprocess.run(["chmod", "-R", "u+w", path], stderr=subprocess.DEVNULL)
        shutil.rmtree(path, ignore_errors=True)


class ProjectRun:
    def __init__(self, project, args):
        self.p = project
        self.args = args
        self.base = os.path.join(args.work, project["name"])
        self.bins = {"genuine": args.genuine, "rs": args.bazel_rs}
        self.res = {"commit": project["commit"], "tools": {t: {} for t in TOOLS}, "compare": {}}

    def state(self, tool):
        return os.path.join(self.base, tool)

    def ws(self, tool):
        return os.path.join(self.base, tool, "ws", self.p["subdir"])

    def env(self, tool):
        home = os.path.join(self.state(tool), "home")
        tmp = os.path.join(self.state(tool), "tmp")
        for d in (home, tmp):
            os.makedirs(d, exist_ok=True)
        return {
            "PATH": "/usr/bin:/bin",
            "HOME": home,
            "TMPDIR": tmp,
            "USER": os.environ.get("USER", "coder"),
            "LANG": "C.UTF-8",
        }

    def prepare(self):
        src = os.path.join(self.args.src, self.p["name"])
        for tool in TOOLS:
            rmtree_force(self.state(tool))
            shutil.copytree(src, os.path.join(self.state(tool), "ws"), symlinks=True,
                            ignore=shutil.ignore_patterns(".git"))
            apply_edits(self.p, self.ws(tool))
            os.makedirs(os.path.join(self.state(tool), "logs"), exist_ok=True)

    def run(self, tool, phase, command, flags, targets, batch=True, tag=None):
        argv = tool_argv(tool, self.bins[tool], self.state(tool), command, flags, targets, batch)
        log = os.path.join(self.state(tool), "logs", tag or phase)
        print(f"  [{self.p['name']}] {tool} {tag or phase}: {' '.join(argv[1:])}", flush=True)
        rec = run_cmd(argv, self.ws(tool), self.env(tool), log, self.args.timeout)
        rec["log"] = log
        if tag != "server2":
            print(f"    exit={rec['exit']} wall={rec['wall']}s rss={fmt_mb(rec)}MB", flush=True)
        return rec

    def server_hwm(self):
        root = os.path.join(self.state("genuine"), "root")
        for dirpath, _dirs, files in os.walk(root):
            if "server.pid.txt" in files:
                with open(os.path.join(dirpath, "server.pid.txt"), encoding="utf-8") as fh:
                    return proc_hwm_kb(int(fh.read().strip()))
            if dirpath.count(os.sep) - root.count(os.sep) > 3:
                _dirs[:] = []
        return None

    def bazel_bin(self, tool):
        argv = tool_argv(tool, self.bins[tool], self.state(tool), "info", self.p["flags"], [], True)
        argv.append("bazel-bin")
        out = subprocess.run(["nice", "-n", "10", *argv], cwd=self.ws(tool), env=self.env(tool),
                             capture_output=True, text=True)
        return out.stdout.strip().splitlines()[-1] if out.returncode == 0 and out.stdout.strip() else None

    def go(self, phases):
        flags, targets = self.p["flags"], self.p["targets"]
        nb = ["--nobuild", "--keep_going"]
        for phase in phases:
            if phase not in self.p["phases"]:
                continue
            if phase in ("nobuild_cold", "nobuild_warm"):
                for tool in TOOLS:
                    self.res["tools"][tool][phase] = self.run(tool, phase, "build", nb + flags, targets)
            elif phase == "server":
                self.run("genuine", phase, "build", nb + flags, targets, batch=False, tag="server1")
                rec = self.run("genuine", phase, "build", nb + flags, targets, batch=False, tag="server2")
                rec["rss_kb"] = self.server_hwm()
                print(f"    exit={rec['exit']} wall={rec['wall']}s server rss={fmt_mb(rec)}MB", flush=True)
                self.res["tools"]["genuine"][phase] = rec
                self.run("genuine", phase, "shutdown", [], [], batch=False, tag="shutdown")
            elif phase == "query":
                qflags = self.p.get("query_flags", [f for f in flags if f.startswith(("--deleted_packages", "--check_direct"))])
                labels = {}
                for tool in TOOLS:
                    rec = self.run(tool, phase, "query", qflags + ["--keep_going"], ["//..."])
                    with open(rec["log"] + ".out", encoding="utf-8", errors="replace") as fh:
                        labels[tool] = parse_query_labels(fh.read())
                    rec["count"] = len(labels[tool])
                    self.res["tools"][tool][phase] = rec
                self.res["compare"]["query"] = compare_sets(labels["genuine"], labels["rs"])
            elif phase == "build":
                for tool in TOOLS:
                    self.res["tools"][tool][phase] = self.run(tool, phase, "build", ["--keep_going"] + flags, targets)
                self.res["compare"]["outputs"] = self.compare_outputs()
            elif phase == "test":
                sts = {}
                for tool in TOOLS:
                    rec = self.run(tool, phase, "test", ["--keep_going"] + flags, self.p["test_targets"])
                    text = ""
                    for ext in (".out", ".err"):
                        with open(rec["log"] + ext, encoding="utf-8", errors="replace") as fh:
                            text += fh.read()
                    sts[tool] = parse_test_statuses(text)
                    self.res["tools"][tool][phase] = rec
                self.res["compare"]["tests"] = compare_statuses(sts["genuine"], sts["rs"])
        return self.res

    def compare_outputs(self):
        lists, roots = {}, {}
        for tool in TOOLS:
            root = self.bazel_bin(tool)
            roots[tool] = root
            lists[tool] = list_outputs(root) if root and os.path.isdir(root) else []
        params = {t: [p for p in lists[t] if PARAMS_RE.search(p)] for t in TOOLS}
        for t in TOOLS:
            lists[t] = [p for p in lists[t] if not PARAMS_RE.search(p)]
        cmp_ = compare_sets(lists["genuine"], lists["rs"])
        cmp_["params_files"] = {t: len(params[t]) for t in TOOLS}
        cmp_["roots"] = roots
        common = sorted(set(lists["genuine"]) & set(lists["rs"]))
        mism = []
        for rel in sample_paths(common):
            hg = output_digest(os.path.join(roots["genuine"], rel))
            hr = output_digest(os.path.join(roots["rs"], rel))
            if hg != hr:
                mism.append(rel)
        cmp_["sampled"] = min(len(common), SAMPLE_SIZE)
        cmp_["hash_mismatches"] = len(mism)
        cmp_["hash_mismatch_examples"] = mism[:20]
        return cmp_


def list_outputs(root, max_depth=48):
    """Relative paths of the files below bazel-bin, minus runfiles trees.
    Directory symlinks (tools link tree artifacts and node_modules
    differently) are followed; only a cycle back into an ancestor stops."""
    out = []

    def walk(directory, rel, ancestors):
        try:
            entries = sorted(os.scandir(directory), key=lambda e: e.name)
        except OSError:
            return
        for entry in entries:
            sub = f"{rel}/{entry.name}" if rel else entry.name
            try:
                is_dir = entry.is_dir(follow_symlinks=True)
            except OSError:
                continue
            if is_dir:
                if entry.name.endswith(".runfiles") or len(ancestors) >= max_depth:
                    continue
                real = os.path.realpath(entry.path)
                if real in ancestors:
                    continue
                walk(entry.path, sub, ancestors | {real})
            elif entry.is_file(follow_symlinks=True):
                out.append(sub)

    if os.path.isdir(root):
        walk(root, "", frozenset({os.path.realpath(root)}))
    return sorted(out)


def normalize_manifest(data):
    """A runfiles manifest's runfiles paths (the targets are absolute paths
    into each tool's own output tree, so they always differ)."""
    keys = []
    for line in data.decode("utf-8", "replace").splitlines():
        keys.append(line.split(" ", 1)[0] if line.strip() else line)
    return "\n".join(sorted(keys)).encode()


def output_digest(path):
    if path.endswith((".runfiles_manifest", "/MANIFEST")) or os.path.basename(path) == "MANIFEST":
        try:
            with open(path, "rb") as fh:
                return hashlib.sha256(normalize_manifest(fh.read())).hexdigest()
        except OSError as exc:
            return f"error:{exc.errno}"
    return sha256_file(path)


def sha256_file(path):
    h = hashlib.sha256()
    try:
        with open(path, "rb") as fh:
            for chunk in iter(lambda: fh.read(1 << 20), b""):
                h.update(chunk)
    except OSError as exc:
        return f"error:{exc.errno}"
    return h.hexdigest()


def load_results(path):
    if os.path.exists(path):
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    return {}


def save_results(path, results):
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(results, fh, indent=1, sort_keys=True)
    os.replace(tmp, path)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", default=DEFAULT_CONFIG)
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("clone")
    c.add_argument("--src", required=True)
    c.add_argument("--only")
    r = sub.add_parser("run")
    r.add_argument("--src", required=True)
    r.add_argument("--work", required=True)
    r.add_argument("--genuine", required=True)
    r.add_argument("--bazel-rs", required=True)
    r.add_argument("--only")
    r.add_argument("--phases", default=",".join(PHASES))
    r.add_argument("--jobs", type=int, default=16)
    r.add_argument("--timeout", type=int, default=1200)
    r.add_argument("--results")
    r.add_argument("--keep", action="store_true", help="keep the per-tool state after the run")
    t = sub.add_parser("table")
    t.add_argument("--results", required=True)
    args = ap.parse_args(argv)
    cfg = load_config(args.config)
    if args.cmd == "table":
        print(render_table(load_results(args.results)))
        return 0
    projects = select(cfg["projects"], args.only)
    if args.cmd == "clone":
        require_abs(args.src, "--src")
        for p in projects:
            print(f"clone {p['name']} @ {p['commit'][:12]}", flush=True)
            clone(p, args.src)
        return 0
    for path, what in ((args.src, "--src"), (args.work, "--work"), (args.genuine, "--genuine"), (args.bazel_rs, "--bazel-rs")):
        require_abs(path, what)
    results_path = args.results or os.path.join(args.work, "results.json")
    install = os.path.join(args.work, "genuine-install")
    if not os.path.isdir(install):
        os.makedirs(args.work, exist_ok=True)
        subprocess.run([args.genuine, f"--install_base={install}", "--output_user_root=" + os.path.join(args.work, "genuine-version-root"),
                        "--batch", "--ignore_all_rc_files", "version"], cwd="/", check=True, capture_output=True)
    phases = [ph for ph in args.phases.split(",") if ph]
    for ph in phases:
        if ph not in PHASES:
            sys.exit(f"unknown phase {ph}")
    for p in projects:
        # Local execution only: no disk or remote cache from the project's rc.
        p = dict(p, flags=[f"--jobs={args.jobs}", "--disk_cache=", "--remote_cache=", *p["flags"]])
        run = ProjectRun(p, args)
        run.prepare()
        res = run.go(phases)
        results = load_results(results_path)
        results[p["name"]] = merge_result(results.get(p["name"]), res)
        save_results(results_path, results)
        logs = os.path.join(args.work, "logs", p["name"])
        rmtree_force(logs)
        for tool in TOOLS:
            shutil.copytree(os.path.join(run.state(tool), "logs"), os.path.join(logs, tool))
        if not args.keep:
            rmtree_force(run.base)
    print(render_table(load_results(results_path)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
