#!/usr/bin/env python3
"""Like-for-like benchmark of bazel-rs against genuine Bazel 9.3 on public
repositories (docs/BENCHMARKS.md describes the method and the results).

usage: bench_genuine.py run --config CONFIG.json --work DIR --bazel GENUINE_ELF
                            --bazel-rs BAZEL_RS [--repo NAME ...]
                            [--scenario NAME ...] [--reps N] [--jobs J]
       bench_genuine.py report --work DIR [--out REPORT.md]

`run` prepares each configured repository (a pinned public git commit plus
the configured edits), gives every side its OWN copy of the workspace and its
own output base, and measures these scenarios, interleaving the sides and
rotating their order in every repetition:

  cold        fresh workspace copy, output base, repository cache and HOME;
              `build TARGETS` (fetch + analysis + execution).
  noop        `build TARGETS` again after a successful build.
  source_edit append a fresh line to one source file, `build TARGETS`.
  build_edit  append a fresh target to one BUILD file, `build TARGETS`.
  query       `query QUERY`.
  test        `test TESTS` with every test result cached.
  startup     `version`: genuine extraction into a fresh install base, a
              cold server start and a warm server, versus bazel-rs.

Sides: gs = genuine, server mode (the server stays warm between commands),
gs_restart = genuine server after `shutdown` (a cold JVM, persisted action
cache), gb = genuine `--batch`, rs = bazel-rs. Every command runs under
`env -i PATH=/usr/bin:/bin HOME=<private> TMPDIR=<private>`, at nice +10,
with --ignore_all_rc_files and a fixed --jobs.

Measured per run: wall time; the client's max RSS (wait4 rusage: the largest
process of the client's waited-for tree, i.e. the batch JVM for gb and
bazel-rs or one of its actions for rs); bazel-rs's own peak RSS (VmHWM of the
bazel-rs process, polled); the genuine server's VmHWM for the command (reset
through /proc/<pid>/clear_refs before it). Correctness: exit codes, the
top-level output files of the verification build (genuine: BEP
namedSetOfFiles; bazel-rs: the `label<TAB>path` stdout lines) compared by
relative path and SHA-256, query output, test outcomes.

All paths must be absolute; the script refuses relative --work/--bazel paths.
Never point it at a private repository: genuine Bazel runs on every workspace.
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

SIDES_ALL = ("gs", "gs_restart", "gb", "rs")
SCENARIOS = ("startup", "cold", "noop", "source_edit", "build_edit", "query", "test")
# Sides measured per scenario (startup has its own sub-sides).
SCENARIO_SIDES = {
    "cold": ("gs", "gb", "rs"),
    "noop": ("gs", "gs_restart", "gb", "rs"),
    "source_edit": ("gs", "gb", "rs"),
    "build_edit": ("gs", "gb", "rs"),
    "query": ("gs", "gs_restart", "gb", "rs"),
    "test": ("gs", "gs_restart", "gb", "rs"),
    "startup": ("g_extract", "g_server_cold", "g_server_warm", "rs"),
}
WORKSPACE_OF = {"gs": "gs", "gs_restart": "gs", "gb": "gb", "rs": "rs"}


# ---------------------------------------------------------------- pure parts


def percentile(samples, pct):
    """Nearest-rank percentile (pct in 0..100) of a non-empty sample list."""
    if not samples:
        raise ValueError("no samples")
    ordered = sorted(samples)
    rank = max(1, -(-len(ordered) * pct // 100))  # ceil(n * pct / 100)
    return ordered[int(rank) - 1]


def median(samples):
    if not samples:
        raise ValueError("no samples")
    ordered = sorted(samples)
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[mid]
    return (ordered[mid - 1] + ordered[mid]) / 2


def summarize(samples):
    return {
        "n": len(samples),
        "median": median(samples),
        "p95": percentile(samples, 95),
        "min": min(samples),
        "max": max(samples),
    }


def rotated(items, rep):
    """The side order of repetition `rep`: rotate by rep to spread bias."""
    items = list(items)
    if not items:
        return items
    k = rep % len(items)
    return items[k:] + items[:k]


def edit_text(template, counter):
    """The text appended by an edit scenario; `{n}` becomes the counter."""
    return template.replace("{n}", str(counter))


def normalize_label(label):
    """`@@//pkg:t` / `@//pkg:t` -> `//pkg:t` (main-repository labels)."""
    for prefix in ("@@//", "@//"):
        if label.startswith(prefix):
            return label[len(prefix) - 2 :]
    return label


def relative_output_path(path, workspace):
    """Workspace-relative path of an output (exec-root relative for derived
    files): strips `.../execroot/_main/`, `.../outputs/` (bazel-rs action
    cache) or the workspace prefix."""
    if path.startswith("file://"):
        path = path[len("file://") :]
    for marker in ("/execroot/_main/", "/outputs/"):
        idx = path.find(marker)
        if idx >= 0:
            return path[idx + len(marker) :]
    ws = workspace.rstrip("/") + "/"
    if path.startswith(ws):
        return path[len(ws) :]
    return path


def parse_bep_outputs(lines):
    """{label: [uri, ...]} of the default output group from BEP JSON lines."""
    named = {}
    completed = {}
    for line in lines:
        line = line.strip()
        if not line:
            continue
        event = json.loads(line)
        eid = event.get("id", {})
        if "namedSet" in eid and "namedSetOfFiles" in event:
            body = event["namedSetOfFiles"]
            named[eid["namedSet"]["id"]] = (
                [f["uri"] for f in body.get("files", []) if "uri" in f],
                [s["id"] for s in body.get("fileSets", [])],
            )
        elif "targetCompleted" in eid and "completed" in event:
            label = normalize_label(eid["targetCompleted"]["label"])
            sets = []
            for group in event["completed"].get("outputGroup", []):
                if group.get("name") == "default":
                    sets.extend(s["id"] for s in group.get("fileSets", []))
            completed.setdefault(label, []).extend(sets)

    def expand(set_id, seen):
        if set_id in seen or set_id not in named:
            return []
        seen.add(set_id)
        files, children = named[set_id]
        out = list(files)
        for child in children:
            out.extend(expand(child, seen))
        return out

    result = {}
    for label, sets in completed.items():
        seen = set()
        uris = []
        for set_id in sets:
            uris.extend(expand(set_id, seen))
        result[label] = uris
    return result


def parse_rs_outputs(text):
    """{label: [path, ...]} from bazel-rs build stdout (`label<TAB>path`)."""
    result = {}
    for line in text.splitlines():
        if "\t" not in line:
            continue
        label, path = line.split("\t", 1)
        if not label.startswith(("//", "@")):
            continue
        result.setdefault(normalize_label(label), []).append(path)
    return result


TEST_LINE = re.compile(r"^(//\S+|@\S+)\s+(\(cached\) )?(PASSED|FAILED|FLAKY|TIMEOUT|NO STATUS|SKIPPED)\b")


def parse_test_summary(text):
    """{label: status} and the `Executed ...` summary line of a test run."""
    statuses = {}
    summary = ""
    for line in text.splitlines():
        m = TEST_LINE.match(line.strip())
        if m:
            statuses[normalize_label(m.group(1))] = m.group(3)
        elif line.startswith("Executed ") or line.startswith("There were no tests"):
            summary = line.strip()
    return statuses, summary


def compare_outputs(a, b):
    """Differences between two {label: {relpath: sha}} maps (list of str)."""
    diffs = []
    for label in sorted(set(a) | set(b)):
        if label not in a:
            diffs.append(f"{label}: only in second")
            continue
        if label not in b:
            diffs.append(f"{label}: only in first")
            continue
        fa, fb = a[label], b[label]
        for rel in sorted(set(fa) | set(fb)):
            if rel not in fa:
                diffs.append(f"{label}: {rel} only in second")
            elif rel not in fb:
                diffs.append(f"{label}: {rel} only in first")
            elif fa[rel] != fb[rel]:
                diffs.append(f"{label}: {rel} content differs")
    return diffs


def fmt_s(value):
    return f"{value:.2f}"


def fmt_mb(kb):
    return f"{kb / 1024:.0f}"


def render_table(rows, headers):
    out = ["| " + " | ".join(headers) + " |", "|" + "---|" * len(headers)]
    for row in rows:
        out.append("| " + " | ".join(str(c) for c in row) + " |")
    return "\n".join(out)


# ------------------------------------------------------------- process layer


def read_text(path):
    with open(path) as fh:
        return fh.read()


def write_text(path, text):
    with open(path, "w") as fh:
        fh.write(text)


def require_abs(path, what):
    if not os.path.isabs(path):
        sys.exit(f"{what} must be an absolute path: {path}")
    return path


def proc_hwm_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as fh:
            for line in fh:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1])
    except OSError:
        return None
    return None


def reset_hwm(pid):
    try:
        with open(f"/proc/{pid}/clear_refs", "w") as fh:
            fh.write("5")
        return True
    except OSError:
        return False


def run_cmd(argv, cwd, env, stdout_path, stderr_path, timeout, poll_hwm=False):
    """Runs argv at nice +10; returns wall seconds, exit code, rusage maxrss
    (kB) and the polled VmHWM of the direct child (kB, or None)."""
    with open(stdout_path, "wb") as out, open(stderr_path, "wb") as err:
        start = time.monotonic()
        proc = subprocess.Popen(
            argv,
            cwd=cwd,
            env=env,
            stdout=out,
            stderr=err,
            stdin=subprocess.DEVNULL,
            preexec_fn=lambda: os.nice(10),
        )
        hwm = [None]
        stop = threading.Event()

        def poll():
            while not stop.is_set():
                value = proc_hwm_kb(proc.pid)
                if value is not None:
                    hwm[0] = value
                stop.wait(0.02)

        poller = threading.Thread(target=poll, daemon=True) if poll_hwm else None
        if poller:
            poller.start()
        deadline = start + timeout
        while True:
            pid, status, rusage = os.wait4(proc.pid, os.WNOHANG)
            if pid:
                break
            if time.monotonic() > deadline:
                proc.send_signal(signal.SIGKILL)
                pid, status, rusage = os.wait4(proc.pid, 0)
                break
            time.sleep(0.005)
        wall = time.monotonic() - start
        stop.set()
        if poller:
            poller.join()
        proc.returncode = os.waitstatus_to_exitcode(status)
    return wall, proc.returncode, rusage.ru_maxrss, hwm[0]


class Repo:
    def __init__(self, cfg, work, args):
        self.cfg = cfg
        self.name = cfg["name"]
        self.work = work
        self.args = args
        self.root = os.path.join(work, self.name)
        self.pristine = os.path.join(self.root, "pristine")
        self.results = []
        self.counter = int(time.time()) % 100000 * 100
        self.server_pids = {}

    # -- layout
    def ws(self, side):
        return os.path.join(self.root, "ws-" + WORKSPACE_OF[side])

    def state(self, side):
        return os.path.join(self.root, "state-" + WORKSPACE_OF[side])

    def env(self, side):
        st = self.state(side)
        home, tmp = os.path.join(st, "home"), os.path.join(st, "tmp")
        os.makedirs(home, exist_ok=True)
        os.makedirs(tmp, exist_ok=True)
        return {"PATH": "/usr/bin:/bin", "HOME": home, "TMPDIR": tmp}

    def argv(self, side, command, extra):
        st = self.state(side)
        if side == "rs":
            return [
                self.args.bazel_rs,
                "--ignore_all_rc_files",
                "--cache-dir=" + os.path.join(st, "cache"),
                command,
            ] + extra
        startup = [
            self.args.bazel,
            "--ignore_all_rc_files",
            "--output_user_root=" + os.path.join(st, "root"),
            "--install_base=" + os.path.join(self.work, "install-base"),
        ]
        if side == "gb":
            startup.append("--batch")
        return startup + [command] + extra

    def flags(self):
        return ["--jobs=%d" % self.args.jobs] + self.cfg.get("flags", [])

    # -- preparation
    def prepare(self):
        if os.path.isdir(self.pristine):
            return
        src = self.cfg["source"]
        clone = os.path.join(self.root, "clone")
        shutil.rmtree(clone, ignore_errors=True)
        os.makedirs(clone)
        git = ["git", "-C", clone]
        subprocess.run(git + ["init", "-q"], check=True)
        subprocess.run(git + ["fetch", "-q", "--depth", "1", src["git"], src["commit"]], check=True)
        subprocess.run(git + ["checkout", "-q", "FETCH_HEAD"], check=True)
        tree = os.path.join(clone, src.get("subdir", ""))
        shutil.copytree(tree, self.pristine, symlinks=True, ignore=shutil.ignore_patterns(".git"))
        for rel in self.cfg.get("delete", []):
            target = os.path.join(self.pristine, rel)
            if os.path.isdir(target):
                shutil.rmtree(target)
            elif os.path.lexists(target):
                os.remove(target)
        for rel in self.cfg.get("delete_named", []):
            for dirpath, _dirs, files in os.walk(os.path.join(self.pristine, rel["under"])):
                if rel["name"] in files:
                    os.remove(os.path.join(dirpath, rel["name"]))
        for patch in self.cfg.get("replace", []):
            path = os.path.join(self.pristine, patch["file"])
            text = read_text(path)
            if patch["old"] not in text:
                sys.exit(f"{self.name}: replace target not found in {patch['file']}")
            write_text(path, text.replace(patch["old"], patch["new"]))
        shutil.rmtree(clone)

    def reset_workspace(self, side):
        ws = self.ws(side)
        shutil.rmtree(ws, ignore_errors=True)
        shutil.copytree(self.pristine, ws, symlinks=True)

    def shutdown(self, side):
        if side == "rs" or side == "gb":
            return
        root = os.path.join(self.state(side), "root")
        if not os.path.isdir(root) or not os.path.isdir(self.ws(side)):
            return
        subprocess.run(
            self.argv(side, "shutdown", []),
            cwd=self.ws(side),
            env=self.env(side),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=300,
        )

    def wipe_state(self, side):
        self.shutdown(side)
        st = self.state(side)
        if os.path.isdir(st):
            subprocess.run(["chmod", "-R", "u+w", st], check=False)
            shutil.rmtree(st, ignore_errors=True)

    def server_pid(self, side):
        root = os.path.join(self.state(side), "root")
        if not os.path.isdir(root):
            return None
        for entry in os.listdir(root):
            pid_file = os.path.join(root, entry, "server", "server.pid.txt")
            if os.path.exists(pid_file):
                try:
                    pid = int(read_text(pid_file).strip())
                except ValueError:
                    continue
                if os.path.exists(f"/proc/{pid}"):
                    return pid
        return None

    # -- one command
    def execute(self, scenario, side, rep, command, extra, measured=True, tag=""):
        outdir = os.path.join(self.root, "runs", scenario)
        os.makedirs(outdir, exist_ok=True)
        stem = f"{side}-{rep}{tag}" if measured else f"{side}-prime{tag}-{rep}"
        argv = self.argv(side, command, extra)
        server = side in ("gs", "gs_restart")
        pid = self.server_pid(side) if server else None
        if pid:
            reset_hwm(pid)
        wall, code, maxrss, hwm = run_cmd(
            argv,
            self.ws(side),
            self.env(side),
            os.path.join(outdir, stem + ".out"),
            os.path.join(outdir, stem + ".err"),
            self.args.timeout,
            poll_hwm=(side == "rs"),
        )
        server_hwm = None
        if server:
            pid = self.server_pid(side)
            server_hwm = proc_hwm_kb(pid) if pid else None
        record = {
            "repo": self.name,
            "scenario": scenario,
            "side": side,
            "rep": rep,
            "measured": measured,
            "command": command,
            "argv": argv,
            "wall_s": round(wall, 4),
            "exit": code,
            "client_maxrss_kb": maxrss,
            "rs_hwm_kb": hwm,
            "server_hwm_kb": server_hwm,
            "stem": stem,
        }
        self.results.append(record)
        with open(os.path.join(self.root, "results.jsonl"), "a") as fh:
            fh.write(json.dumps(record) + "\n")
        print(
            f"[{self.name}] {scenario:<11} {side:<10} {stem:<14} exit={code} wall={wall:7.2f}s "
            f"client={maxrss // 1024}MB rs_hwm={(hwm or 0) // 1024}MB server={(server_hwm or 0) // 1024}MB",
            flush=True,
        )
        return record

    def build_args(self):
        return self.flags() + ["--"] + self.cfg["build_targets"]

    def test_args(self):
        return self.flags() + ["--"] + self.cfg["test_targets"]

    # -- verification
    def verify_outputs(self):
        """Builds once more per side (genuine with a BEP file) and compares the
        top-level default outputs of gs/gb/rs by relative path and SHA-256."""
        maps = {}
        for side in ("gs", "gb", "rs"):
            extra = list(self.build_args())
            bep = os.path.join(self.root, f"verify-{side}.bep.json")
            if side != "rs":
                extra = ["--build_event_json_file=" + bep] + extra
            rec = self.execute("verify", side, 0, "build", extra, measured=False)
            outdir = os.path.join(self.root, "runs", "verify")
            if side == "rs":
                text = read_text(os.path.join(outdir, rec["stem"] + ".out"))
                raw = parse_rs_outputs(text)
            else:
                raw = parse_bep_outputs(read_text(bep).splitlines())
            files = {}
            for label, paths in raw.items():
                entry = files.setdefault(label, {})
                for path in paths:
                    local = path[len("file://") :] if path.startswith("file://") else path
                    rel = relative_output_path(local, self.ws(side))
                    entry[rel] = sha256_path(local)
            maps[side] = files
        verdict = {
            "gs_vs_rs": compare_outputs(maps["gs"], maps["rs"]),
            "gs_vs_gb": compare_outputs(maps["gs"], maps["gb"]),
            "labels": {s: len(m) for s, m in maps.items()},
            "files": {s: sum(len(v) for v in m.values()) for s, m in maps.items()},
        }
        with open(os.path.join(self.root, "verify.json"), "w") as fh:
            json.dump(verdict, fh, indent=1, sort_keys=True)
        print(f"[{self.name}] verify: {len(verdict['gs_vs_rs'])} gs/rs diffs, labels {verdict['labels']}", flush=True)
        return verdict


def sha256_path(path):
    if os.path.isdir(path):
        digest = hashlib.sha256()
        for dirpath, dirs, files in os.walk(path):
            dirs.sort()
            for name in sorted(files):
                full = os.path.join(dirpath, name)
                digest.update(os.path.relpath(full, path).encode() + b"\0")
                digest.update(sha256_path(full).encode())
        return "dir:" + digest.hexdigest()
    try:
        with open(path, "rb") as fh:
            return hashlib.sha256(fh.read()).hexdigest()
    except OSError as exc:
        return f"error:{exc.errno}"


# --------------------------------------------------------------- scenarios


def scenario_startup(repo, reps):
    for rep in range(reps):
        for side in rotated(SCENARIO_SIDES["startup"], rep):
            if side == "rs":
                repo.execute("startup", "rs", rep, "version", [])
                continue
            install = os.path.join(repo.root, "startup-install")
            root = os.path.join(repo.root, "startup-root")
            base = [repo.args.bazel, "--ignore_all_rc_files", "--output_user_root=" + root]
            if side == "g_extract":
                # fresh install base and output root + batch: extraction and
                # one JVM start (never under the running server's install base).
                fresh = os.path.join(repo.root, "startup-extract")
                shutil.rmtree(fresh, ignore_errors=True)
                argv = [repo.args.bazel, "--ignore_all_rc_files",
                        "--output_user_root=" + os.path.join(fresh, "root"),
                        "--install_base=" + os.path.join(fresh, "install"), "--batch", "version"]
            elif side == "g_server_cold":
                subprocess.run(base + ["--install_base=" + install, "shutdown"], cwd=repo.ws("gs"),
                               env=repo.env("gs"), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                argv = base + ["--install_base=" + install, "version"]
            else:
                argv = base + ["--install_base=" + install, "version"]
            if not os.path.isdir(install) and side != "g_extract":
                subprocess.run(base + ["--install_base=" + install, "--batch", "version"],
                               cwd=repo.ws("gs"), env=repo.env("gs"),
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            outdir = os.path.join(repo.root, "runs", "startup")
            os.makedirs(outdir, exist_ok=True)
            wall, code, maxrss, _ = run_cmd(argv, repo.ws("gs"), repo.env("gs"),
                                            os.path.join(outdir, f"{side}-{rep}.out"),
                                            os.path.join(outdir, f"{side}-{rep}.err"), 600)
            record = {"repo": repo.name, "scenario": "startup", "side": side, "rep": rep,
                      "measured": True, "command": "version", "argv": argv, "wall_s": round(wall, 4),
                      "exit": code, "client_maxrss_kb": maxrss, "rs_hwm_kb": None,
                      "server_hwm_kb": None, "stem": f"{side}-{rep}"}
            repo.results.append(record)
            with open(os.path.join(repo.root, "results.jsonl"), "a") as fh:
                fh.write(json.dumps(record) + "\n")
            print(f"[{repo.name}] startup     {side:<14} exit={code} wall={wall:7.2f}s", flush=True)
    root = os.path.join(repo.root, "startup-root")
    subprocess.run([repo.args.bazel, "--ignore_all_rc_files", "--output_user_root=" + root,
                    "--install_base=" + os.path.join(repo.root, "startup-install"), "shutdown"],
                   cwd=repo.ws("gs"), env=repo.env("gs"), stdout=subprocess.DEVNULL,
                   stderr=subprocess.DEVNULL)


def ensure_built(repo):
    """Fresh-but-primed state for the warm scenarios: every side has built."""
    for side in ("gs", "gb", "rs"):
        if not os.path.isdir(repo.ws(side)):
            repo.reset_workspace(side)
        repo.execute("prime", side, 0, "build", repo.build_args(), measured=False)


def scenario_cold(repo, reps):
    for rep in range(reps):
        for side in rotated(SCENARIO_SIDES["cold"], rep):
            repo.wipe_state(side)
            repo.reset_workspace(side)
            repo.execute("cold", side, rep, "build", repo.build_args())


def scenario_warm(repo, scenario, command, extra, reps, warmups=1):
    sides = SCENARIO_SIDES[scenario]
    for side in sides:
        if side == "gs_restart":
            continue
        for w in range(warmups):
            repo.execute(scenario, side, w, command, extra, measured=False)
    for rep in range(reps):
        for side in rotated(sides, rep):
            if side == "gs_restart":
                repo.shutdown("gs")
            repo.execute(scenario, side, rep, command, extra)


def scenario_edit(repo, scenario, reps):
    spec = repo.cfg[scenario]
    sides = SCENARIO_SIDES[scenario]
    for side in sides:  # warm-up: one edit + build per side, unmeasured
        apply_edit(repo, side, spec, repo.counter)
        repo.execute(scenario, side, 0, "build", repo.build_args(), measured=False)
    for rep in range(reps):
        repo.counter += 1
        for side in rotated(sides, rep):
            apply_edit(repo, side, spec, repo.counter)
            repo.execute(scenario, side, rep, "build", repo.build_args())
    for side in sides:  # restore the pristine file and rebuild
        restore_file(repo, side, spec["file"])
        repo.execute(scenario, side, 99, "build", repo.build_args(), measured=False, tag="-restore")


def apply_edit(repo, side, spec, counter):
    pristine = read_text(os.path.join(repo.pristine, spec["file"]))
    with open(os.path.join(repo.ws(side), spec["file"]), "w") as fh:
        fh.write(pristine + edit_text(spec["append"], counter))


def restore_file(repo, side, rel):
    shutil.copy2(os.path.join(repo.pristine, rel), os.path.join(repo.ws(side), rel))


def run_repo(repo, scenarios, reps):
    repo.prepare()
    try:
        if "startup" in scenarios:
            for side in ("gs", "rs"):
                if not os.path.isdir(repo.ws(side)):
                    repo.reset_workspace(side)
            scenario_startup(repo, reps)
        if "cold" in scenarios:
            scenario_cold(repo, repo.cfg.get("cold_reps", reps))
        if set(scenarios) & {"noop", "source_edit", "build_edit", "query", "test"}:
            ensure_built(repo)
            repo.verify_outputs()
        if "noop" in scenarios:
            scenario_warm(repo, "noop", "build", repo.build_args(), reps)
        if "source_edit" in scenarios:
            scenario_edit(repo, "source_edit", reps)
        if "build_edit" in scenarios:
            scenario_edit(repo, "build_edit", reps)
        if "query" in scenarios:
            scenario_warm(repo, "query", "query", repo.cfg.get("query_flags", []) + [repo.cfg["query"]], reps)
        if "test" in scenarios and repo.cfg.get("test_targets"):
            scenario_warm(repo, "test", "test", repo.test_args(), reps)
    finally:
        for side in ("gs",):
            repo.shutdown(side)


# ------------------------------------------------------------------ report


def load_results(work):
    results = []
    for name in sorted(os.listdir(work)):
        path = os.path.join(work, name, "results.jsonl")
        if os.path.exists(path):
            with open(path) as fh:
                results.extend(json.loads(line) for line in fh if line.strip())
    return results


def correctness_notes(work, results):
    notes = []
    by_key = {}
    for r in results:
        if r["measured"]:
            by_key.setdefault((r["repo"], r["scenario"], r["rep"]), {})[r["side"]] = r
    for (repo, scenario, rep), sides in sorted(by_key.items()):
        codes = {s: r["exit"] for s, r in sides.items()}
        if len(set(codes.values())) > 1:
            notes.append(f"{repo} {scenario} rep {rep}: exit codes differ {codes}")
    for repo in sorted({r["repo"] for r in results}):
        vpath = os.path.join(work, repo, "verify.json")
        if os.path.exists(vpath):
            verdict = json.loads(read_text(vpath))
            diffs = verdict["gs_vs_rs"]
            notes.append(
                f"{repo} outputs gs vs rs: {len(diffs)} differences "
                f"(labels {verdict['labels']}, files {verdict['files']})"
                + ("" if not diffs else ": " + "; ".join(diffs[:12]))
            )
        # query output and test outcomes, last measured rep
        for scenario in ("query", "test"):
            outs = {}
            for r in results:
                if r["repo"] == repo and r["scenario"] == scenario and r["measured"]:
                    path = os.path.join(work, repo, "runs", scenario, r["stem"] + ".out")
                    if os.path.exists(path):
                        outs[r["side"]] = read_text(path)
            if "gs" in outs and "rs" in outs:
                if scenario == "query":
                    same = sorted(outs["gs"].split()) == sorted(outs["rs"].split())
                    notes.append(f"{repo} query output gs vs rs: {'identical (as sets)' if same else 'DIFFERENT'}")
                else:
                    sg, lg = parse_test_summary(outs["gs"])
                    sr, lr = parse_test_summary(outs["rs"])
                    same = sg == sr
                    notes.append(f"{repo} test outcomes gs vs rs: {'identical' if same else 'DIFFERENT'} "
                                 f"(genuine: '{lg}'; bazel-rs: '{lr}')")
    return notes


def build_report(work):
    results = load_results(work)
    lines = ["# bazel-rs vs genuine Bazel 9.3 — raw benchmark report", ""]
    for repo in sorted({r["repo"] for r in results}):
        lines += [f"## {repo}", ""]
        rows = []
        for scenario in SCENARIOS:
            for side in SCENARIO_SIDES[scenario]:
                rs = [r for r in results if r["repo"] == repo and r["scenario"] == scenario
                      and r["side"] == side and r["measured"]]
                if not rs:
                    continue
                walls = [r["wall_s"] for r in rs]
                st = summarize(walls)
                bad = sum(1 for r in rs if r["exit"] != 0)
                mem = []
                for r in rs:
                    if side.startswith("gs"):
                        mem.append((r["server_hwm_kb"] or 0) + r["client_maxrss_kb"])
                    elif side == "rs":
                        mem.append(r["rs_hwm_kb"] or r["client_maxrss_kb"])
                    else:
                        mem.append(r["client_maxrss_kb"])
                rows.append([
                    scenario, side, st["n"], fmt_s(st["median"]), fmt_s(st["p95"]),
                    fmt_s(st["min"]), fmt_s(st["max"]), fmt_mb(max(mem)),
                    bad, " ".join(fmt_s(w) for w in walls),
                ])
        lines.append(render_table(rows, ["scenario", "side", "n", "median s", "p95 s", "min s",
                                         "max s", "peak RSS MB", "nonzero exits", "raw samples s"]))
        lines.append("")
    lines += ["## Correctness", ""]
    lines += ["- " + n for n in correctness_notes(work, results)]
    lines.append("")
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    run = sub.add_parser("run")
    run.add_argument("--config", required=True)
    run.add_argument("--work", required=True)
    run.add_argument("--bazel", required=True)
    run.add_argument("--bazel-rs", required=True)
    run.add_argument("--repo", action="append", default=[])
    run.add_argument("--scenario", action="append", default=[])
    run.add_argument("--reps", type=int, default=5)
    run.add_argument("--jobs", type=int, default=16)
    run.add_argument("--timeout", type=int, default=3600)
    rep = sub.add_parser("report")
    rep.add_argument("--work", required=True)
    rep.add_argument("--out")
    args = parser.parse_args(argv)
    if args.cmd == "report":
        text = build_report(require_abs(args.work, "--work"))
        if args.out:
            with open(args.out, "w") as fh:
                fh.write(text)
        else:
            print(text)
        return 0
    for path, what in ((args.work, "--work"), (args.bazel, "--bazel"), (args.bazel_rs, "--bazel-rs"),
                       (args.config, "--config")):
        require_abs(path, what)
    if os.path.realpath(args.work).startswith("/home/coder/code"):
        sys.exit("refusing a work directory inside /home/coder/code")
    config = json.loads(read_text(args.config))
    scenarios = args.scenario or list(SCENARIOS)
    for name in scenarios:
        if name not in SCENARIOS:
            sys.exit(f"unknown scenario {name}")
    os.makedirs(args.work, exist_ok=True)
    for cfg in config["repos"]:
        if args.repo and cfg["name"] not in args.repo:
            continue
        run_repo(Repo(cfg, args.work, args), scenarios, args.reps)
    return 0


if __name__ == "__main__":
    sys.exit(main())
