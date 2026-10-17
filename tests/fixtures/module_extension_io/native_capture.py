"""Native-only diagnostic collection. No reference launch or implicit paired PASS.

Loaded only by the owned validator; no imports of legacy QA. Q is that validator's
byte/inventory/process API, not a simulated Starlark host or fabricated API result.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import stat
import sys
import time

Q = None
OUTPUT = "bazel-out/k8-fastbuild/bin/consumed.json"  # rust-fastbuild before N41 closed D1
CANONICAL_REPOSITORY = "+io+made"
# N36 manifest format (src/action.rs MANIFEST_FORMAT); the action KEY framing
# is still native-action-v5 and is not observable in the cache layout.
ACTION_MANIFEST_FORMAT = "native-action-v7"
NATIVE_VERSION = b"bazel-rs 0.1.0 (experimental; compatibility target Bazel 9.3.0)\n"
SUMMARY = re.compile(rb"bazel-rs: ([0-9]+) actions executed, ([0-9]+) cache hits \(experimental local execution; not sandboxed\)")
SCHEMA = "module-io-native-diagnostic-v2"
FS_LIMITS = {"entries": 10000, "depth": 64, "file_bytes": 64 * 1024 * 1024,
             "artifact_file_bytes": 8 * 1024 * 1024, "snapshot_bytes": 64 * 1024 * 1024,
             "total_bytes": 1536 * 1024 * 1024, "seconds": 1200}
RUNTIME_SCHEMA = "module-io-runtime-manifest-v2"
COVERAGE_POLICY = {"name": "bazel-rs-runtime-inputs", "version": 1}
RUNTIME_SCOPE = "Versioned runtime coverage only; Python QA and unreviewed external inputs excluded; not Cargo dependency discovery."
REQUIRED_RUNTIME_FILES = (".envrc", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml")
REQUIRED_RUNTIME_ROOTS = ("src", "vendor", "resources", "tests")
PYTHON_SUFFIXES = frozenset({".py", ".pyi", ".pyc", ".pyo"})
# Independent per source-binding pass, sharing the caller's absolute deadline.
# No captured-artifact8MiB limit applies to the native ELF/embedded archives.
# Current inventory measurement:1649 walk visits,28,755,418 selected source bytes,
# largest source22,266,812 bytes; the unchanged64MiB source-file cap also fits ELF.
SOURCE_BINDING_LIMITS = dict(FS_LIMITS)


class RuntimeViolation(RuntimeError):
    """Positive content/result, request policy, publication, or resource failure."""


def configure(qa):
    global Q
    Q = qa


class FSBudget:
    """Cooperative bounded filesystem I/O, not a hostile-same-UID hard deadline.

    Entries and declared scan bytes are charged incrementally. Actual read bytes
    have a separate cumulative cap. Every syscall/read and completed scan checks
    the same absolute deadline. An open descriptor's identity/size/timestamps must
    remain unchanged; no directory symlink or special file is followed.
    """
    def __init__(self, limits=None, *, deadline=None, clock=time.monotonic):
        self.limits = dict(FS_LIMITS if limits is None else limits)
        if set(self.limits) != set(FS_LIMITS) or any(type(v) is not int or v <= 0 for v in self.limits.values()):
            raise ValueError("typed explicit filesystem limits required")
        self.clock = clock
        self.deadline = clock() + self.limits["seconds"] if deadline is None else deadline
        self.entries = self.scanned_bytes = self.read_bytes = 0

    def check(self):
        if self.clock() >= self.deadline:
            raise RuntimeViolation("filesystem deadline exceeded")

    def charge(self):
        self.check()
        self.entries += 1
        if self.entries > self.limits["entries"]:
            raise RuntimeViolation("filesystem entry limit exceeded")

    @staticmethod
    def signature(info):
        return (info.st_dev, info.st_ino, info.st_mode, info.st_size, info.st_mtime_ns, info.st_ctime_ns, info.st_nlink)

    def checked_path(self, path, boundary=None):
        path = Path(path).absolute()
        if ".." in path.parts:
            raise RuntimeViolation("traversing filesystem path")
        boundary = Path(boundary).absolute() if boundary is not None else Path(path.anchor)
        if not path.is_relative_to(boundary) or len(path.relative_to(boundary).parts) > self.limits["depth"]:
            raise RuntimeViolation("filesystem boundary/depth limit")
        # Check each ancestor without resolving through a symlink. Cooperative
        # race detection is also performed on the final opened file below.
        current = Path(path.anchor)
        for part in path.parts[1:-1]:
            self.check()
            current /= part
            info = current.lstat()
            self.check()
            if not stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode):
                raise RuntimeViolation("non-directory/symlink filesystem ancestor")
        return path

    def inspect(self, path, boundary=None):
        self.charge()
        path = self.checked_path(path, boundary)
        info = path.lstat()
        self.check()
        if not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
            raise RuntimeViolation("symlink/special native filesystem entry")
        if stat.S_ISREG(info.st_mode):
            self.scanned_bytes += info.st_size
            if self.scanned_bytes > self.limits["total_bytes"]:
                raise RuntimeViolation("filesystem scanned-byte limit exceeded")
        return info

    def walk(self, root, *, missing=False):
        root = Path(root).absolute()
        try:
            info = self.inspect(root, root)
        except FileNotFoundError:
            if missing:
                self.check()
                return
            raise
        if not stat.S_ISDIR(info.st_mode):
            raise RuntimeViolation("filesystem walk root is not a directory")

        def visit(path, relative, info):
            self.check()
            yield path, relative, info
            if stat.S_ISDIR(info.st_mode):
                with os.scandir(path) as children:
                    self.check()
                    for child in children:
                        name = relative + "/" + child.name if relative else child.name
                        Q.inventory_key(name)
                        if len(Path(name).parts) > self.limits["depth"]:
                            raise RuntimeViolation("filesystem depth limit exceeded")
                        child_info = self.inspect(Path(child.path), root)
                        yield from visit(Path(child.path), name, child_info)
                    self.check()
                if self.signature(path.lstat()) != self.signature(info):
                    raise RuntimeViolation("directory changed during bounded walk")
                self.check()
        yield from visit(root, "", info)
        self.check()

    def read(self, path, *, boundary=None, limit=None, keep=True):
        limit = min(self.limits["file_bytes"], limit if limit is not None else self.limits["file_bytes"])
        path = self.checked_path(path, boundary)
        before = self.inspect(path, boundary)
        if not stat.S_ISREG(before.st_mode) or before.st_size > limit:
            raise RuntimeViolation("regular file/read-size limit required")
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        fd = os.open(path, flags)
        chunks, sha, size = [], hashlib.sha256(), 0
        try:
            opened = os.fstat(fd)
            if self.signature(opened) != self.signature(before):
                raise RuntimeViolation("file changed between stat and open")
            while True:
                self.check()
                part = os.read(fd, min(65536, limit - size + 1))
                self.check()
                if not part:
                    break
                size += len(part)
                self.read_bytes += len(part)
                if size > before.st_size or size > limit or self.read_bytes > self.limits["total_bytes"]:
                    raise RuntimeViolation("filesystem read growth/byte limit exceeded")
                sha.update(part)
                if keep:
                    chunks.append(part)
            if size != before.st_size or self.signature(os.fstat(fd)) != self.signature(before):
                raise RuntimeViolation("file changed during bounded read")
        finally:
            os.close(fd)
        if self.signature(path.lstat()) != self.signature(before):
            raise RuntimeViolation("file replaced during bounded read")
        self.check()
        pin = {"sha256": sha.hexdigest(), "size": size, "mode": format(stat.S_IMODE(before.st_mode), "04o")}
        return (b"".join(chunks) if keep else None), pin

    def pin(self, path):
        return self.read(path, keep=False)[1]

    def files(self, root, *, missing=False):
        # The list is constructed ONLY after each incremental charge, never by
        # materializing rglob/listdir before checking a limit.
        result = []
        for path, _, info in self.walk(root, missing=missing):
            if stat.S_ISREG(info.st_mode):
                result.append(path)
        self.check()
        return result

    def inventory(self, root):
        result = {}
        for path, relative, info in self.walk(root):
            if stat.S_ISREG(info.st_mode) and relative != "SHA256SUMS.json":
                result[relative] = self.read(path, boundary=root, keep=False)[1]
        self.check()
        return result


def budget_for(frozen=None, *, deadline=None, snapshot=False):
    limits = dict(FS_LIMITS)
    if frozen is not None:
        Q.equal(frozen["fs_limits"], FS_LIMITS, "explicit frozen filesystem limits; no waiver/enlargement")
    if snapshot:
        limits["total_bytes"] = limits["snapshot_bytes"]
    return FSBudget(limits, deadline=deadline)


def bounded_blob(value):
    if (type(value) is not dict or type(value.get("size")) is not int or not 0 <= value["size"] <= FS_LIMITS["artifact_file_bytes"]
            or type(value.get("base64")) is not str or len(value["base64"]) > ((FS_LIMITS["artifact_file_bytes"] + 2) // 3) * 4):
        raise RuntimeViolation("embedded native artifact size/type limit")
    return Q.unblob(value)


def copy_input(path, destination, budget):
    data, pin = budget.read(path)
    destination.parent.mkdir(parents=True, exist_ok=True)
    budget.check()
    destination.write_bytes(data)
    destination.chmod(int(pin["mode"], 8))
    budget.check()


class OwnedRoot:
    """Exclusive ownership starts before any initializer creates scratch."""
    def __init__(self, path, *, parent=Path("/tmp/opencode")):
        self.path, self.parent = Path(path).absolute(), Path(parent).absolute()
        self.identity = None
        self.sock = self.runner = None

    def __enter__(self):
        if (self.path.parent != self.parent or self.parent.resolve() != self.parent
                or not self.path.name.startswith("module-extension-io-native-")
                or self.path.exists() or self.path.is_symlink()):
            raise ValueError("new exclusive owned native artifact root required")
        self.path.mkdir(mode=0o700)
        info = self.path.lstat()
        self.identity = (info.st_dev, info.st_ino)
        return self

    def cleanup(self):
        result = {"scratch_absent": False, "all_groups_absent": True, "errors": []}
        if self.sock is not None:
            self.sock.close()
        groups = []
        if self.runner is not None:
            groups = list(self.runner.groups)
            if self.runner.server is not None:
                groups.append(self.runner.server.pid)
            active = [getattr(self.runner, "active_process", None), self.runner.server]
            for process in active:
                if process is None:
                    continue
                try:
                    if process.poll() is None or Q.process_group_exists(process.pid):
                        Q.kill_group(process)
                    else:
                        process.wait(timeout=10)
                except Exception as error:
                    result["errors"].append(str(error))
            result["all_groups_absent"] = all(not Q.process_group_exists(pid) for pid in groups)
        try:
            info = self.path.lstat()
            if not stat.S_ISDIR(info.st_mode) or (info.st_dev, info.st_ino) != self.identity:
                raise RuntimeViolation("owned artifact root identity changed; refusing cleanup traversal")
            # A separate, explicit cooperative cleanup window is not an extension
            # of the operation deadline. Failure is retained, never called clean.
            budget = FSBudget({**FS_LIMITS, "seconds": 30})
            scratch = self.path / "scratch"

            def remove(path, depth):
                budget.charge()
                if depth > budget.limits["depth"]:
                    raise RuntimeViolation("cleanup depth limit")
                budget.checked_path(path, self.path)
                try:
                    current = path.lstat()
                except FileNotFoundError:
                    return
                if stat.S_ISDIR(current.st_mode):
                    with os.scandir(path) as entries:
                        for child in entries:
                            budget.check()
                            remove(Path(child.path), depth + 1)
                    path.rmdir()
                else:
                    path.unlink()  # unlink links themselves; never traverse targets
                budget.check()
            remove(scratch, 0)
            result["scratch_absent"] = not scratch.exists() and not scratch.is_symlink()
        except (Exception, KeyboardInterrupt) as error:
            result["errors"].append(type(error).__name__ + ": " + str(error))
        return result

    def __exit__(self, kind, error, traceback):
        cleanup = self.cleanup()
        if error is not None or cleanup["errors"] or not cleanup["all_groups_absent"] or not cleanup["scratch_absent"]:
            receipt = {"state": "NATIVE_INITIALIZATION_OR_CLEANUP_BLOCKER", "error_type": kind.__name__ if kind else None,
                       "error": str(error)[:4096] if error else None, "cleanup": cleanup}
            info = self.path.lstat() if self.path.exists() and not self.path.is_symlink() else None
            destination = (self.path / "initialization-failure.json" if info and (info.st_dev, info.st_ino) == self.identity
                           else self.parent / (self.path.name + "-failed-cleanup.json"))
            # Exclusive receipt creation cannot overwrite an unrelated path.
            with destination.open("x", encoding="ascii") as stream:
                stream.write(json.dumps(receipt, sort_keys=True, ensure_ascii=True) + "\n")
            if error is None:
                raise RuntimeViolation("owned initialization/cleanup did not finish cleanly")
        return False


def source_budget(parent=None):
    if parent is not None:
        parent.check()
    return FSBudget(SOURCE_BINDING_LIMITS, deadline=parent.deadline if parent is not None else None)


def reviewed_selectors(value, label):
    if type(value) is not list or any(type(path) is not str for path in value):
        raise ValueError(label + " must be a canonical path list")
    for path in value:
        Q.inventory_key(path)
    if len(set(value)) != len(value):
        raise ValueError("duplicate " + label)
    return value


def enumerate_runtime_sources(root, additional_files=(), additional_roots=(), budget=None):
    """Bounded exact policy coverage, deliberately not Cargo dependency discovery."""
    budget = budget or source_budget()
    root = Path(root).absolute()
    files = set()
    for name in REQUIRED_RUNTIME_FILES:
        info = budget.inspect(root / name, root)
        require(stat.S_ISREG(info.st_mode), "mandatory runtime input is not regular: " + name)
        files.add(name)
    for name in (*REQUIRED_RUNTIME_ROOTS, ".cargo"):
        for path, _, info in budget.walk(root / name, missing=name == ".cargo"):
            if stat.S_ISREG(info.st_mode) and (path.suffix == ".rs" if name == "tests" else path.suffix.lower() not in PYTHON_SUFFIXES):
                files.add(str(path.relative_to(root)))
    try:
        build_info = budget.inspect(root / "build.rs", root)
    except FileNotFoundError:
        pass
    else:
        require(stat.S_ISREG(build_info.st_mode), "root build.rs must be regular")
        files.add("build.rs")
    extra_files = reviewed_selectors(list(additional_files), "reviewed additional files")
    extra_roots = reviewed_selectors(list(additional_roots), "reviewed additional roots")
    for index, name in enumerate(extra_roots):
        if any(Path(name).is_relative_to(other) or Path(other).is_relative_to(name) for other in extra_roots[:index]):
            raise ValueError("overlapping reviewed additional roots")
        if any(Path(name).is_relative_to(other) for other in ("src", "vendor", "resources", ".cargo")):
            raise ValueError("reviewed additional root is already wholly covered")
        for path, _, info in budget.walk(root / name):
            if stat.S_ISREG(info.st_mode):
                require(path.suffix.lower() not in PYTHON_SUFFIXES, "reviewed directory contains Python; narrow its explicit coverage")
                files.add(str(path.relative_to(root)))
    for name in extra_files:
        if name in files or any(Path(name).is_relative_to(other) for other in extra_roots):
            raise ValueError("redundant reviewed additional file")
        require(Path(name).suffix.lower() not in PYTHON_SUFFIXES, "Python is outside runtime coverage")
        info = budget.inspect(root / name, root)
        require(stat.S_ISREG(info.st_mode), "reviewed additional input must be regular")
        files.add(name)
    budget.check()
    return files


def read_runtime_ledger(path, expected_sha, budget, expected_root=None):
    raw, pin = budget.read(path)
    Q.equal(pin["sha256"], expected_sha, "external runtime ledger pin")
    ledger = Q.load_json(raw)
    if ledger.get("schema") != RUNTIME_SCHEMA:
        raise ValueError("unversioned/unsupported runtime ledger is historical only; strict v2 ledger required")
    Q.equal(ledger.get("coverage_policy"), COVERAGE_POLICY, "supported versioned runtime coverage policy")
    Q.equal(ledger.get("binding_scope"), RUNTIME_SCOPE, "count-neutral runtime binding scope")
    if type(ledger.get("worktree")) is not str or type(ledger.get("native_path")) is not str:
        raise ValueError("typed runtime worktree/native paths required")
    root = Path(ledger["worktree"])
    if not root.is_absolute() or ".." in root.parts or str(root) != ledger["worktree"]:
        raise ValueError("canonical absolute runtime worktree required")
    executable = Path(ledger["native_path"])
    if not executable.is_absolute() or ".." in executable.parts or str(executable) != ledger["native_path"]:
        raise ValueError("canonical absolute native path required")
    if type(ledger.get("native_sha256")) is not str or not re.fullmatch(r"[0-9a-f]{64}", ledger["native_sha256"]):
        raise ValueError("canonical native digest required")
    if expected_root is not None:
        Q.equal(str(root), str(expected_root), "runtime worktree binding")
    sources, count = ledger["runtime_sources"], ledger["runtime_source_count"]
    if type(sources) is not dict or type(count) is not int or count <= 0 or count != len(sources):
        raise ValueError("runtime_source_count must be positive integer and exactly match source map")
    for name, checksum in sources.items():
        Q.inventory_key(name)
        if Path(name).suffix.lower() in PYTHON_SUFFIXES or type(checksum) is not str or not re.fullmatch(r"[0-9a-f]{64}", checksum):
            raise ValueError("runtime source path/hash/Python scope")
    reviewed_selectors(ledger.get("reviewed_additional_files", []), "reviewed additional files")
    reviewed_selectors(ledger.get("reviewed_additional_roots", []), "reviewed additional roots")
    return ledger, pin


def ledger_membership(ledger, budget):
    actual = enumerate_runtime_sources(ledger["worktree"], ledger.get("reviewed_additional_files", []),
                                       ledger.get("reviewed_additional_roots", []), budget)
    Q.equal(sorted(actual), sorted(ledger["runtime_sources"]), "exact runtime coverage membership")
    return actual


def runtime_generation(ledger, ledger_sha, native_sha, members):
    return {"schema": RUNTIME_SCHEMA, "coverage_policy": dict(COVERAGE_POLICY), "ledger_sha256": ledger_sha,
            "native_sha256": native_sha, "source_count": len(members),
            "sources_sha256": Q.digest(Q.canonical(ledger["runtime_sources"])),
            "source_membership_sha256": Q.digest(Q.canonical(sorted(members))), "scope": RUNTIME_SCOPE}


def validate_runtime_generation(binding):
    if type(binding) is not dict:
        raise ValueError("v2 frozen runtime generation required")
    Q.equal(binding.get("schema"), RUNTIME_SCHEMA, "runtime generation schema")
    Q.equal(binding.get("coverage_policy"), COVERAGE_POLICY, "runtime generation coverage policy")
    Q.equal(binding.get("scope"), RUNTIME_SCOPE, "runtime generation count-neutral scope")
    if type(binding.get("source_count")) is not int or binding["source_count"] <= 0:
        raise ValueError("typed measured runtime count required")
    for key in ("ledger_sha256", "native_sha256", "sources_sha256", "source_membership_sha256"):
        if type(binding.get(key)) is not str or not re.fullmatch(r"[0-9a-f]{64}", binding[key]):
            raise ValueError("runtime generation digest")


def verify_frozen_membership(frozen, parent_budget=None):
    budget = source_budget(parent_budget)
    binding = frozen["runtime_binding"]
    validate_runtime_generation(binding)
    ledger, _ = read_runtime_ledger(frozen["runtime_manifest_path"], binding["ledger_sha256"], budget, Q.ROOT)
    members = ledger_membership(ledger, budget)
    Q.equal(runtime_generation(ledger, binding["ledger_sha256"], binding["native_sha256"], members), binding, "same frozen runtime generation")
    for name, checksum in ledger["runtime_sources"].items():
        Q.equal(frozen["pins"][str(Path(ledger["worktree"]) / name)]["sha256"], checksum, "frozen source pin coverage")
    budget.check()
    if parent_budget is not None:
        parent_budget.check()


def runtime_binding(path, expected_sha, native, native_sha, expected_root=None, budget=None):
    parent = budget
    budget = source_budget(parent)
    path, native = Path(path).absolute(), Path(native).absolute()
    ledger, ledger_pin = read_runtime_ledger(path, expected_sha, budget, expected_root)
    root = Path(ledger["worktree"])
    members = ledger_membership(ledger, budget)
    Q.equal(str(native), ledger["native_path"], "runtime native path")
    Q.equal(native_sha, ledger["native_sha256"], "runtime release declared pin")
    native_bytes, native_pin = budget.read(native)
    Q.equal(native_pin["sha256"], native_sha, "actual native release bytes")
    Q.equal(native_bytes[:4].hex(), "7f454c46", "native must be direct pinned ELF")
    sources = ledger["runtime_sources"]
    pins = {str(path): ledger_pin, str(native): native_pin}
    for relative, checksum in sources.items():
        source = root / relative
        pin = budget.pin(source)
        Q.equal(pin["sha256"], checksum, "frozen runtime source drift: " + relative)
        pins[str(source)] = pin
    budget.check()
    Q.equal(sorted(ledger_membership(ledger, budget)), sorted(members), "runtime membership drift during binding")
    if parent is not None:
        parent.check()
    return ledger, pins, runtime_generation(ledger, expected_sha, native_sha, members)


def deferred_lanes():
    source = Q.load_json(budget_for().read(Q.FIXTURES / "deferred.json")[0])
    lanes = []
    for group in ("reference_instrumentation", "native_zero_effect_rejections"):
        lanes += [{"lane": group, "name": case["name"], "state": "DEFERRED_NOT_EXECUTED"}
                  for case in source[group]["cases"]]
    lanes += [{"lane": "core_unit_only", "name": name, "state": "DEFERRED_NOT_EXECUTED"}
              for name in source["core_unit_only"]]
    lanes.append({"lane": "tls", "name": "reference-JVM/native-injected-transport/CLI-separated",
                  "state": "DEFERRED_NOT_EXECUTED"})
    return lanes


def plan(root, native):
    root = str(root)
    scratch = root + "/scratch"
    rows = []

    def add(phase, case="", argv=None):
        rows.append({"ordinal": len(rows), "phase": phase, "case": case, "cycle": 0,
                     "purpose": "native-diagnostic-preparation", "engine": "native",
                     "row_type": "event" if argv is None else "command", "argv": argv or [],
                     "cwd": scratch + ("/workspace/" + case if case else ""),
                     "env": Q.environment(scratch), "umask": 0o022, "platform_requests": []})

    for phase in ("pins-start", "runtime-ledger-verified", "public-input-snapshot", "registry-local-copy", "server-start"):
        add(phase)
    add("version", argv=[str(native), "--ignore_all_rc_files", "version"])
    for case in Q.cases():
        add("case-start", case.name)
        for phase in ("cold", "warm") if case.result == "success" else ("negative",):
            argv = [str(native), "--ignore_all_rc_files", "--cache-dir=" + scratch + "/cache/" + case.name,
                    "build", "--jobs=2", "--lockfile_mode=off", "--registry=file://" + root + "/inputs/registry",
                    *case.flags, "//:consume"]
            add(phase, case.name, argv)
            add(phase + "-observation", case.name)
        add("case-end", case.name)
    for phase in ("server-stop", "cleanup", "pins-end"):
        add(phase)
    return rows


def native_events(stderr, case, source, origin):
    if source != Q.render(case, origin)["extension.bzl"]:
        raise ValueError("native callback source differs from frozen recipe")
    events, frames, tail = [], [], []
    ended = False
    for line in stderr.split(b"\n"):
        if not line:
            if ended:
                tail.append(line)
            continue
        line.decode("utf-8", errors="strict")
        if line.startswith(Q.MARKER):
            if ended:
                raise ValueError("native callback frame after terminal diagnostics")
            events.append(Q.load_json(line[len(Q.MARKER):].decode("utf-8", errors="strict")))
            frames.append(line + b"\n")
        else:
            ended = True
            if Q.MARKER in line:
                raise ValueError("native source echo is not a callback observation")
            tail.append(line)
    # Native's pinned StderrPrintHandler emits no DEBUG location. Bind each
    # observation to exact frozen source print sites, explicitly NOT runtime sites.
    sites = [i for i, line in enumerate(source.decode("utf-8").splitlines(), 1)
             if line.startswith('    print("MODULE_IO_JSON " + json.encode(')]
    return {"events": events, "frames": Q.blob(b"".join(frames)), "tail": b"\n".join(tail),
            "site_binding": {"kind": "frozen-fixture-sites-not-emitted-runtime-locations",
                             "source_sha256": Q.digest(source), "print_lines": sites}}


def diagnostic_message(case):
    return {
        "http-filter-false": "module I/O validation: No URLs left after removing plain http URLs due to missing checksum",
        "allfail-false": "module I/O completion: HTTP status 404",
        "disabled-miss-false": "module I/O completion: repository downloads are disabled",
        "empty-list-false": "module I/O validation: urls not set",
        "empty-list-existing-false": "module I/O validation: urls not set",
        "malformed-sha-transfer-success": "module I/O validation: invalid SHA256 checksum",
        "read-watch-yes": "unsupported module I/O profile: watch=yes on extension-owned files",
        "read-missing": "module I/O completion: missing owned file",
        "read-invalid-watch": "module I/O validation: watch must be auto, yes, or no",
    }[case.name]


def frame_snippet(statement):
    """Pinned starlark_syntax 0.14.2 frame.rs:45-53/67 (max_len 80, chars)."""
    snippet = statement.strip()
    if len(snippet) <= 77 or len(snippet) - 77 <= 3:
        return snippet
    return snippet[:77] + "..."


def negative_diagnostic(tail, case, source):
    calls = [(i, line) for i, line in enumerate(source.decode("utf-8").splitlines(), 1)
             if line.startswith("    value = ctx.")]
    Q.equal(len(calls), len(case.operations), "native negative operation count")
    number, statement = calls[-1]
    method = case.operations[-1]["method"]
    if not statement.startswith("    value = ctx." + method + "("):
        raise ValueError("native negative wrong source operation")
    column = statement.index("ctx.") + 1
    digits = len(str(number))
    label = "//:extension.bzl"
    gutter = " " * (digits + 1) + "|"
    lines = ["bazel-rs: module extension " + label + ": Traceback (most recent call last):",
             # Pinned starlark_syntax 0.14.2 frame.rs:78-80 prints location-less
             # (native-invoked) frames; the extension impl is called from Rust.
             "  File <builtin>, in <module>",
             f"  * {label}:{number}, in _impl", "      " + frame_snippet(statement),
             "error: " + diagnostic_message(case),
             " " * digits + f"--> {label}:{number}:{column}", gutter,
             f"{number} | {statement}", gutter + " " * column + "^" * (len(statement) - column + 1), gutter]
    pattern = re.escape("\n".join(lines).encode("utf-8")) + rb"\n{1,2}"
    if not re.fullmatch(pattern, tail):
        raise ValueError("native diagnostic wording/site/phase differs from frozen source profile")


def snapshot_file(path, boundary, budget=None, walked=None):
    """One regular file's bytes/mode.

    Standalone (``walked`` is None) a file must be single-link: nothing proves
    where its other names live. Inside ``snapshot_tree`` the walk's own lstat
    is passed and a hardlinked file is accepted only after the whole walk has
    proven that EVERY name of the inode is inside the snapshotted root
    (N36 publishes action outputs as hardlinks of ``cas/<sha256>-<mode>``).
    """
    budget = budget or budget_for(snapshot=True)
    data, pin = budget.read(path, boundary=boundary, limit=budget.limits["artifact_file_bytes"])
    after = path.lstat()
    if walked is None:
        if after.st_nlink != 1:
            raise RuntimeViolation("nonregular/linked native publication: " + str(path))
    elif FSBudget.signature(after) != FSBudget.signature(walked):
        raise RuntimeViolation("native publication changed between walk and read: " + str(path))
    budget.check()
    return {"mode": pin["mode"], "data": Q.blob(data)}


def snapshot_tree(root, budget, *, missing=False):
    """Bytes/modes of every entry plus the measured in-root hardlink groups.

    Symlinks, devices, FIFOs and sockets are still rejected by the walk. A
    regular file with st_nlink > 1 is accepted only if exactly st_nlink names
    of that (st_dev, st_ino) were walked below ``root``: no name of a native
    cache inode may live outside the cache (or the files of another case).
    ``links`` lists each such group (sorted relative names); it is evidence,
    retained copies are deliberately independent single-link files.
    """
    files, directories, inodes = {}, {}, {}
    for path, relative, info in budget.walk(root, missing=missing):
        if stat.S_ISDIR(info.st_mode):
            directories[relative] = format(stat.S_IMODE(info.st_mode), "04o")
        else:
            files[relative] = snapshot_file(path, root, budget, walked=info)
            if info.st_nlink != 1:
                inodes.setdefault((info.st_dev, info.st_ino), (info.st_nlink, []))[1].append(relative)
    links = []
    for nlink, names in inodes.values():
        budget.check()
        if len(names) != nlink:
            raise RuntimeViolation("linked native publication has names outside the snapshotted cache: "
                                   + str(Path(root) / sorted(names)[0]) + f" ({len(names)} of {nlink} links inside)")
        links.append(sorted(names))
    budget.check()
    return {"cache_exists": "" in directories, "files": files, "directories": directories, "links": sorted(links)}


CAS_BLOB = re.compile(r"cas/([0-9a-f]{64})-([0-7]{3})")


def blob_mode_valid(mode):
    """N36 blob modes: read-only 0444 plus a subset of the executable bits."""
    return mode & 0o444 == 0o444 and mode & ~0o555 == 0


def link_groups(snapshot):
    """Validate recorded in-cache hardlink groups against the recorded files.

    Each group is >= 2 distinct recorded files with identical bytes and mode
    (one inode necessarily has one content and one mode), containing exactly
    one ``cas/<sha256>-<mode>`` blob, every other name being a published action
    output (``actions/<key>/outputs/...``). Anything else (a linked repository
    tree file, manifest, lock, two blobs on one inode) is not N36's design.
    Groups are optional: a copy/rename fallback publication has none.
    """
    require("links" in snapshot, "native link-group evidence required")
    groups, files = snapshot["links"], snapshot["files"]
    require(type(groups) is list, "typed native link groups")
    seen = set()
    for group in groups:
        require(type(group) is list and len(group) >= 2 and group == sorted(set(group)), "canonical native link group")
        for name in group:
            Q.inventory_key(name)
            require(name in files and name not in seen, "native link group names a non-file or repeats a name")
            seen.add(name)
        require(all(files[name] == files[group[0]] for name in group), "native link group bytes/mode differ")
        blobs = [name for name in group if CAS_BLOB.fullmatch(name)]
        require(len(blobs) == 1, "native link group must contain exactly one content-addressed blob")
        require(all(re.fullmatch(r"actions/[0-9a-f]{64}/outputs/.+", name) for name in group if name != blobs[0]),
                "native link group aliases a non-output cache file")
    require(groups == sorted(groups), "canonical native link group order")
    return groups


def snapshot_entries(snapshot, prefix=""):
    """Reconstruct entries from measured directories and actual retained bytes."""
    files, directories = snapshot["files"], snapshot["directories"]
    require(type(files) is dict and type(directories) is dict, "typed native file/directory maps")
    require(len(files) + len(directories) <= FS_LIMITS["entries"], "native snapshot entry limit")
    require(type(snapshot["cache_exists"]) is bool, "typed cache-existence evidence")
    require(snapshot["cache_exists"] == ("" in directories), "cache root directory evidence")
    require(not (set(files) & set(directories)), "native file/directory overlap")
    if not snapshot["cache_exists"]:
        require(not files and not directories, "absent cache has physical entries")
    all_entries, total = {}, 0
    for name in list(directories) + list(files):
        if name:
            Q.inventory_key(name)
            require(len(Path(name).parts) <= FS_LIMITS["depth"], "snapshot path depth")
            parent = str(Path(name).parent)
            parent = "" if parent == "." else parent
            require(parent in directories, "native snapshot missing directory metadata")
        mode = directories[name] if name in directories else files[name]["mode"]
        require(type(mode) is str and bool(re.fullmatch(r"0[0-7]{3}", mode)), "typed native mode")
        if name in directories:
            entry = {"directory": True, "mode": int(mode, 8), "size": 0, "sha256": ""}
        else:
            require(name != "", "cache root cannot be a file")
            Q.equal(sorted(files[name]), ["data", "mode"], "native file observation schema")
            data = bounded_blob(files[name]["data"])
            total += len(data)
            require(total <= FS_LIMITS["snapshot_bytes"], "snapshot total byte limit")
            entry = {"directory": False, "mode": int(mode, 8), "size": len(data), "sha256": Q.digest(data)}
        all_entries[name] = entry
    if not prefix:
        return all_entries
    Q.inventory_key(prefix)
    require(prefix in directories, "missing actual repository directory metadata")
    return {name[len(prefix) + 1:] if name != prefix else "": entry for name, entry in all_entries.items()
            if name == prefix or name.startswith(prefix + "/")}


def tree_entries(root):
    return snapshot_entries(snapshot_tree(root, budget_for(snapshot=True)))


def publication_entries(snapshot):
    entries = snapshot_entries(snapshot)
    # N36: cas/ (the content-addressed store whose blobs back published action
    # outputs) is a publication root too, so a failed callback cannot leave
    # blobs and every positive blob must be accounted for.
    roots = ("repositories/direct/trees", "repositories/direct/manifests", "actions", "cas")
    # Empty namespace parents/locks/registry metadata are not publications.
    # Every descendant (including an empty orphan identity directory) is.
    return sorted(name for name, entry in entries.items() if any(
        name.startswith(prefix + "/") or (name == prefix and not entry["directory"]) for prefix in roots))


def discover_publications(snapshot, cache):
    """One discovery/accounting implementation for live and retained physical data.

    No caller-supplied repository/action list participates in discovery. Every
    physical publication must belong to a verified manifest and its actual files.
    """
    cache = Path(cache)
    entries = snapshot_entries(snapshot)
    result = {"repositories": [], "actions": [], "decode_errors": [],
              "publication_entries": publication_entries(snapshot)}
    accounted = set()
    direct = cache / "repositories/direct"
    for relative in sorted(snapshot["files"]):
        if not re.fullmatch(r"repositories/direct/manifests/[^/]+\.json", relative):
            continue
        path = cache / relative
        manifest_bytes = bounded_blob(snapshot["files"][relative]["data"])
        try:
            raw = Q.load_json(manifest_bytes)
            Q.equal(sorted(raw), ["files", "format", "identity", "profile"], "repository manifest schema")
            identity = raw["identity"]
        except (ValueError, KeyError, TypeError) as error:
            result["decode_errors"].append({"path": str(path), "error": str(error)})
            continue
        if type(identity) is not str or not re.fullmatch(r"[0-9a-f]{64}", identity) or path.name != identity + ".json":
            result["decode_errors"].append({"path": str(path), "error": "native repository manifest identity/path"})
            continue
        tree = direct / "trees" / identity
        try:
            tree_data = snapshot_entries(snapshot, "repositories/direct/trees/" + identity)
            Q.equal([raw["format"], raw["profile"]], [1, "direct-file-template-v1"], "repository manifest profile")
            Q.equal(raw["files"], tree_data, "discovered repository manifest contradicts physical bytes/modes")
        except (OSError, ValueError, RuntimeViolation) as error:
            result["decode_errors"].append({"path": str(tree), "error": str(error)})
            continue
        result["repositories"].append({"identity": identity, "tree_path": str(tree), "manifest_path": str(path),
            "manifest": Q.blob(manifest_bytes), "actual_entries": tree_data,
            "physical_root": Q.manifest_entry("", "directory", snapshot["directories"]["repositories/direct/trees/" + identity]),
            "canonical_binding": {"name": CANONICAL_REPOSITORY, "kind": "single-frozen-declaration-not-a-query"}})
        accounted.add(relative)
        prefix = "repositories/direct/trees/" + identity
        accounted.update(prefix + "/" + name if name else prefix for name in tree_data)
    for relative, file in sorted(snapshot["files"].items()):
        if re.fullmatch(r"actions/[^/]+/manifest\.json", relative):
            path = cache / relative
            try:
                manifest = Q.load_json(bounded_blob(file["data"]))
                Q.equal(sorted(manifest), ["format", "key", "outputs"], "action manifest schema")
                key = path.parent.name
                require(bool(re.fullmatch(r"[0-9a-f]{64}", key)), "action identity path")
                # The KEY identity stays native-action-v5; the manifest is
                # native-action-v7 since N36 (src/action.rs MANIFEST_FORMAT).
                Q.equal([manifest["format"], manifest["key"]], [ACTION_MANIFEST_FORMAT, key], "action identity/profile")
                require(type(manifest["outputs"]) is list and bool(manifest["outputs"]), "action outputs required")
                claimed = {relative}
                names = set()
                for output in manifest["outputs"]:
                    Q.equal(sorted(output), ["digest", "mode", "name", "symlink", "tree"], "action output schema")
                    # Only regular-file outputs are in this fixture's scope:
                    # a tree or symlink output is a publication this checker
                    # does not account for, never silently accepted.
                    Q.equal([output["tree"], output["symlink"]], [None, None], "regular-file action output")
                    name = bytes.fromhex(output["name"]).decode("utf-8", errors="strict")
                    Q.inventory_key(name)
                    Q.equal(name.encode().hex(), output["name"], "canonical output-name encoding")
                    require(name not in names, "duplicate action output")
                    names.add(name)
                    full = "actions/" + key + "/outputs/" + name
                    actual = entries[full]
                    require(not actual["directory"], "declared action output is not a file")
                    Q.equal([output["digest"], output["mode"], actual["mode"]],
                            [actual["sha256"], 0o555, 0o555], "action output digest/mode")
                    claimed.add(full)
                    parent = Path(full).parent
                    while str(parent) != "actions":
                        require(entries[str(parent)]["directory"], "action output parent is not a directory")
                        claimed.add(str(parent))
                        parent = parent.parent
                accounted.update(claimed)
            except (ValueError, TypeError, KeyError, RuntimeViolation) as error:
                result["decode_errors"].append({"path": str(path), "error": str(error)})
                continue
            result["actions"].append({"key": path.parent.name, "path": str(path), "manifest": file["data"]})
    # N36 store blobs: a regular file directly below cas/ whose name is its
    # own content digest and its own read-only mode. Stale blobs are legitimate
    # (no store GC), so membership needs no manifest reference; a misnamed,
    # wrong-mode, nested or leftover temporary entry stays unaccounted.
    for relative in sorted(snapshot["files"]):
        if not relative.startswith("cas/"):
            continue
        match = CAS_BLOB.fullmatch(relative)
        try:
            require(match is not None, "content-addressed blob name")
            mode = int(match[2], 8)
            require(blob_mode_valid(mode), "content-addressed blob mode profile")
            Q.equal([entries[relative]["sha256"], entries[relative]["mode"]], [match[1], mode],
                    "content-addressed blob digest/mode vs name")
        except (ValueError, TypeError, KeyError, RuntimeViolation) as error:
            result["decode_errors"].append({"path": str(cache / relative), "error": str(error)})
            continue
        accounted.add(relative)
    result["unaccounted_publications"] = sorted(set(result["publication_entries"]) - accounted)
    return result


def verify_publication_discovery(artifacts, snapshot, cache):
    discovered = discover_publications(snapshot, cache)
    for key, value in discovered.items():
        Q.equal(artifacts[key], value, "independent physical publication discovery: " + key)
    return discovered


def observe(root, case, budget=None):
    budget = budget or budget_for(snapshot=True)
    scratch = root / "scratch"
    cache = scratch / "cache" / case.name
    for _, relative, _ in budget.walk(scratch / "tmp", missing=True):
        if relative and Path(relative).parts[0].startswith("bazel-extension-io-"):
            raise RuntimeViolation("native callback scratch lifetime leak")
    snapshot = snapshot_tree(cache, budget, missing=True)
    result = {"cache_root": str(cache), "callback_scratch_absent": True, "action_work_absent": True, **snapshot}
    for relative in list(snapshot["files"]) + list(snapshot["directories"]):
        if relative.startswith("work/"):
            raise RuntimeViolation("native action workdir leak")
        if any(part.startswith((".stage-", ".trash-")) for part in Path(relative).parts):
            raise RuntimeViolation("native publication stage leak")
    # Link-group SHAPE is judged by validate_stage after retention, like every
    # other publication property; only escaping links are rejected here.
    result.update(discover_publications(snapshot, cache))
    budget.check()
    return result


def require(condition, message):
    if not condition:
        raise RuntimeViolation(message)


def positive_artifacts(row, spec, case, root):
    artifacts = row["artifacts"]
    cache = root / "scratch/cache" / case.name
    Q.equal(artifacts["cache_root"], str(cache), "native cache provenance")
    verify_publication_discovery(artifacts, artifacts, cache)
    require(not artifacts["decode_errors"], "native publication manifest decode failure")
    require(not artifacts["unaccounted_publications"], "positive native orphan publication")
    require(len(artifacts["repositories"]) == 1, "positive native repository publication count")
    repo = artifacts["repositories"][0]
    identity = repo["identity"]
    Q.equal(repo["tree_path"], str(cache / "repositories/direct/trees" / identity), "native source tree path")
    Q.equal(repo["manifest_path"], str(cache / "repositories/direct/manifests" / (identity + ".json")), "native manifest path")
    manifest = Q.load_json(bounded_blob(repo["manifest"]))
    Q.equal(sorted(manifest), ["files", "format", "identity", "profile"], "native repository manifest schema")
    Q.equal([manifest["format"], manifest["identity"], manifest["profile"]], [1, identity, "direct-file-template-v1"], "native publication identity")
    reconstructed = snapshot_entries(artifacts, "repositories/direct/trees/" + identity)
    Q.equal(manifest["files"], reconstructed, "native manifest vs independently reconstructed file bytes/modes")
    Q.equal(repo["actual_entries"], reconstructed, "recorded native entries are not independent proof")
    Q.equal(manifest["files"][""], {"directory": True, "mode": 0o755, "size": 0, "sha256": ""}, "exact native root manifest entry")
    Q.equal(repo["physical_root"], Q.manifest_entry("", "directory", "0755"), "native physical root")
    prefix = "repositories/direct/trees/" + identity + "/"
    Q.equal(repo["manifest"], artifacts["files"]["repositories/direct/manifests/" + identity + ".json"]["data"], "actual repository manifest bytes")
    data = artifacts["files"][prefix + "data.json"]
    Q.equal(data["mode"], "0644", "native source mode")
    # Repository files carry Bazel's internal Latin-1 byte strings (one byte per
    # U+0000..U+00FF character), exactly as the reference oracle decodes them.
    logical = Q.load_json(bounded_blob(data["data"]).decode("latin-1", errors="strict"))
    Q.equal(logical, Q.expected_consumer(case), "native real result/digest/size/readback bytes")
    require(len(artifacts["actions"]) == 1, "native positive consumer action count")
    action = artifacts["actions"][0]
    key = action["key"]
    require(bool(re.fullmatch(r"[0-9a-f]{64}", key)), "native action key")
    Q.equal(action["path"], str(cache / "actions" / key / "manifest.json"), "native action manifest provenance")
    Q.equal(action["manifest"], artifacts["files"]["actions/" + key + "/manifest.json"]["data"], "actual action manifest bytes")
    action_manifest = Q.load_json(bounded_blob(action["manifest"]))
    Q.equal(action_manifest, {"format": ACTION_MANIFEST_FORMAT, "key": key, "outputs": [
        {"name": OUTPUT.encode().hex(), "digest": data["data"]["sha256"], "mode": 0o555, "tree": None, "symlink": None}]},
        "native actual action output manifest")
    output = artifacts["files"]["actions/" + key + "/outputs/" + OUTPUT]
    Q.equal(output["mode"], "0555", "native completed action mode")
    Q.equal(output["data"], data["data"], "native independent consumer bytes")
    return {"repository_identity": identity, "action_key": key, "logical": logical,
            "source": data, "output": output, "output_path": str(cache / "actions" / key / "outputs" / OUTPUT)}


def validate_stage(row, spec, case, freeze):
    Q.equal(row["spec"], spec, "native trusted invocation")
    root = Path(freeze["plan"][0]["cwd"]).parent
    source = Q.unblob(freeze["fixtures"][case.name]["extension.bzl"])
    Q.equal(row["artifacts"]["cache_root"], str(root / "scratch/cache" / case.name), "native observed cache boundary")
    links = link_groups(row["artifacts"])
    # HTTP effects are independent physical evidence. Framing/wording mismatches
    # can never downgrade an extra, reordered or wrong-method request to UNMATCHED.
    require([(r.get("method"), r["path"]) for r in row["requests"]] == [("GET", path) for path in case.requests],
            "native observed HTTP method/order/count differs from genuine contract")
    issues = []
    try:
        observations = native_events(Q.unblob(row["stderr"]), case, source, freeze["origin"])
    except (ValueError, UnicodeError) as error:
        observations = {"events": [], "frames": Q.blob(b""), "tail": Q.unblob(row["stderr"]),
                        "site_binding": {"kind": "unmatched", "source_sha256": Q.digest(source)}}
        issues.append("native callback framing: " + str(error))
    events = observations["events"]
    if re.search(rb"(?m)^error: module I/O (?:resource limit:|policy:|interrupted|internal:)", observations["tail"]):
        raise RuntimeViolation("unexpected native resource/policy/interruption/internal failure in bounded33 cases")
    expected = Q.expected_events(case)
    if Q.canonical(events) != Q.canonical(expected):
        # Wrong returned values are a runtime correctness failure, not wording.
        actual_posts = [e for e in events if e.get("event") == "post"]
        expected_posts = [e for e in expected if e.get("event") == "post"]
        if len(actual_posts) == len(expected_posts) and Q.canonical(actual_posts) != Q.canonical(expected_posts):
            raise RuntimeViolation("native callback returned wrong result/readback")
        issues.append("native exact callback sequence/source phase")
    value = None
    if case.result == "success":
        require(row["status"] == 0, "positive native build failed; inspect raw diagnostic before any further case")
        try:
            value = positive_artifacts(row, spec, case, root)
        except (ValueError, KeyError, TypeError, IndexError, UnicodeError) as error:
            raise RuntimeViolation("positive native publication/result/mode: " + str(error)) from error
        wanted_stdout = ("//:consume\t" + value["output_path"] + "\n").encode("utf-8")
        if Q.unblob(row["stdout"]) != wanted_stdout:
            issues.append("native label TAB actual output-path stdout")
        match = SUMMARY.fullmatch(observations["tail"].removesuffix(b"\n"))
        if not match:
            issues.append("native terminal action/cache summary wording")
        elif [int(match[1]), int(match[2])] != ([1, 0] if spec["phase"] == "cold" else [0, 1]):
            raise RuntimeViolation("native cold/warm action/cache counts differ")
    else:
        require(row["status"] != 0, "native planned hard error became success/fallback")
        require(not row["artifacts"]["decode_errors"], "native negative has malformed publication manifest")
        physical = publication_entries(row["artifacts"])
        Q.equal(row["artifacts"]["publication_entries"], physical, "negative physical publication inventory")
        require(not physical, "native negative left physical repository/action publication")
        require(not row["artifacts"]["repositories"] and not row["artifacts"]["actions"], "native failed callback published outputs")
        require(not links, "native negative left hardlinked publications")
        if row["status"] != 1 or Q.unblob(row["stdout"]):
            issues.append("native controlled-negative exit/stdout")
        try:
            negative_diagnostic(observations["tail"], case, source)
        except ValueError as error:
            issues.append(str(error))
    return {"issues": issues, "events": events, "callback_frames": observations["frames"],
            "site_binding": observations["site_binding"], "consumer": value}


def validate_case(case, rows):
    if case.result != "success":
        return []
    cold, warm = rows["cold"], rows["warm"]
    a, b = cold["validation"]["consumer"], warm["validation"]["consumer"]
    Q.equal([a["repository_identity"], a["action_key"], a["source"], a["output"]],
            [b["repository_identity"], b["action_key"], b["source"], b["output"]], "native independent cold/warm publication/cache stability")
    return []


def freeze_inputs(args, sock, owner):
    root = owner.path
    budget = budget_for()
    ledger, runtime_pins, binding = runtime_binding(args.runtime_manifest, args.runtime_manifest_sha256,
        args.native, args.native_sha256, Q.ROOT, budget)
    Q.equal(str(root), str(Path(args.artifact_dir).absolute()), "exclusive initialization root")
    registry = Path(args.registry_input).absolute()
    registry_files = budget.files(registry)
    require((registry / "bazel_registry.json").is_file(), "native registry must be explicit local metadata")
    qa_files = [Q.ROOT / "scripts/validate_module_extension_io.py", Q.ROOT / "tests/test_validate_module_extension_io.py",
                Q.PLATFORMS, *[p for p in budget.files(Q.FIXTURES) if "__pycache__" not in p.parts and p.suffix != ".pyc"]]
    qa_files += [Q.ROOT / "tests/fixtures/builtin_modules/registry/modules/platforms/1.0.0" / name for name in ("source.json", "MODULE.bazel")]
    support = [Path(sys.executable).resolve(), Path("/bin/bash").resolve(), Path("/bin/cat").resolve()]
    pins = {**runtime_pins, **{str(p): budget.pin(p) for p in [*qa_files, *registry_files, *support]}}
    for name in ("home", "tmp", "workspace", "cache"):
        (root / "scratch" / name).mkdir(parents=True, exist_ok=True, mode=0o700)
    for path in qa_files:
        out = root / "inputs/qa" / path.relative_to(Q.ROOT)
        out.parent.mkdir(parents=True, exist_ok=True)
        copy_input(path, out, budget)
    for path in registry_files:
        out = root / "inputs/registry" / path.relative_to(registry)
        out.parent.mkdir(parents=True, exist_ok=True)
        copy_input(path, out, budget)
    (root / "inputs/runtime").mkdir()
    copy_input(args.runtime_manifest, root / "inputs/runtime/manifest.json", budget)
    # Full versioned-coverage ledger is preserved; these critical format/diagnostic sources
    # are also copied as raw public bytes. This is not an all-source QA claim.
    for name in ("src/main.rs", "src/action.rs", "src/extension_ctx.rs", "src/extension_io.rs",
                 "src/extension_download.rs", "src/repository_store.rs", "src/repository.rs", "src/label.rs",
                 "vendor/starlark-0.14.2/src/stdlib/extra.rs"):
        path = Path(ledger["worktree"]) / name
        out = root / "inputs/runtime/sources" / name
        out.parent.mkdir(parents=True, exist_ok=True)
        copy_input(path, out, budget)
    origin = "http://127.0.0.1:" + str(sock.getsockname()[1])
    fixtures = {}
    for case in Q.cases():
        fixture = Q.render(case, origin)
        if any(b"ctx." + method + b"(" in fixture["extension.bzl"] for method in (b"path", b"execute", b"delete")):
            raise ValueError("reference-only instrumentation in native fixture")
        fixtures[case.name] = {name: Q.blob(data) for name, data in fixture.items()}
        for name, data in fixture.items():
            budget.check()
            for directory in (root / "inputs/workspaces" / case.name, root / "scratch/workspace" / case.name):
                directory.mkdir(parents=True, exist_ok=True)
                (directory / name).write_bytes(data)
                budget.check()
    responses = Q.response_plan()
    responses.pop(Q.PLATFORM_ROUTE)  # Native profile needs no platform artifact request.
    responses["/never"] = {"status": 200, "headers": {"Content-Length": str(len(Q.payloads()["versions"]))},
                           "body": Q.blob(Q.payloads()["versions"])}
    Q.save_json(root / "inputs/responses.json", responses)
    rows = plan(root, args.native)
    rows[4].update(engine="fixture", row_type="command", argv=[str(Path(sys.executable).resolve()), "-I",
        str(root / "inputs/qa/tests/fixtures/module_extension_io/http_server.py"), str(sock.fileno()),
        str(root / "inputs/responses.json"), str(root / "requests.jsonl")])
    for path in budget.files(root / "inputs") + budget.files(root / "scratch/workspace"):
        pins[str(path)] = budget.pin(path)
    frozen = {"schema": SCHEMA, "scope": "FROZEN_RUNTIME_V2_PLUS_OWN_QA_NOT_FULL_PAIRED_ACCEPTANCE",
        "mode": "native-diagnostic", "plan": rows, "pins": pins, "runtime_binding": binding,
        "runtime_manifest_path": str(Path(args.runtime_manifest).absolute()), "native": str(Path(args.native).absolute()),
        "qa_sources": {str(p.relative_to(Q.ROOT)): pins[str(p)] for p in qa_files},
        "origin": origin, "fixtures": fixtures, "responses": responses, "case_definitions": [Q.asdict(c) for c in Q.cases()],
        "deferred_lanes": deferred_lanes(), "callback_counts": {"cold": [1], "warm": [1], "negative": [1]},
        "command_timeout_seconds": args.timeout, "aggregate_timeout_seconds": 1200, "scratch_limit_bytes": 1536 * 1024 * 1024,
        "fs_limits": dict(FS_LIMITS), "cleanup_seconds": 30,
        "source_binding_limits": dict(SOURCE_BINDING_LIMITS),
        "native_capabilities_used": ["version", "build", "label-TAB-artifact stdout", "direct repository manifests", "action manifests"],
        "absent_native_proofs": ["cquery", "aquery", "reference execution log", "CLI private HTTPS trust injection"]}
    Q.save_json(root / "freeze.json", frozen)
    budget.check()
    return root, frozen


def runner_class():
    class NativeRunner(Q.CaptureRunner):
        def checkpoint(self):
            budget = budget_for(self.freeze, deadline=self.started + self.freeze["aggregate_timeout_seconds"])
            Q.equal(self.freeze["source_binding_limits"], SOURCE_BINDING_LIMITS, "frozen source-binding limits")
            verify_frozen_membership(self.freeze, budget)
            for path, expected in self.freeze["pins"].items():
                Q.equal(budget.pin(path), expected, "native runtime/QA input drift: " + path)
            if len(self.requests()) > 512:
                raise Q.HealthFailure("native aggregate time/request budget")
            scratch = self.root / "scratch"
            size = 0
            for _, _, info in budget.walk(scratch):
                if stat.S_ISREG(info.st_mode):
                    size += info.st_size
                    if size > self.freeze["scratch_limit_bytes"]:
                        raise RuntimeViolation("native scratch byte limit")
            budget.check()
            verify_frozen_membership(self.freeze, budget)
            elapsed = time.monotonic() - self.started  # AFTER the entire bounded scan
            value = {"pins_sha256": Q.digest(Q.canonical(self.freeze["pins"])), "scratch_bytes": size,
                     "elapsed_seconds": elapsed, "runtime_binding": self.freeze["runtime_binding"]}
            Q.validate_checkpoint(value, self.freeze)
            return value

        def requests(self):
            path = self.root / "requests.jsonl"
            budget = budget_for(self.freeze, deadline=self.started + self.freeze["aggregate_timeout_seconds"])
            try:
                data = budget.read(path, boundary=self.root, limit=FS_LIMITS["artifact_file_bytes"])[0]
            except FileNotFoundError:
                return []
            result = []
            for line in data.split(b"\n"):
                budget.check()
                if line:
                    result.append(Q.load_json(line))
                    if len(result) > 512:
                        raise RuntimeViolation("native request ledger count limit")
            return result
    return NativeRunner


def retain_cache(root, case, phase, artifacts, budget=None):
    budget = budget or budget_for(snapshot=True)
    snapshot_entries(artifacts)
    retained = root / "native-cache" / case.name / phase
    if artifacts["cache_exists"]:
        for name in sorted(artifacts["directories"], key=lambda p: len(Path(p).parts)):
            budget.check()
            (retained / name).mkdir(parents=True, exist_ok=True)
    for name, value in artifacts["files"].items():
        budget.check()
        Q.inventory_key(name)
        path = root / "native-cache" / case.name / phase / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(bounded_blob(value["data"]))
        path.chmod(int(value["mode"], 8))
    for name, mode in sorted(artifacts["directories"].items(), key=lambda item: len(Path(item[0]).parts), reverse=True):
        budget.check()
        (retained / name).chmod(int(mode, 8))
    Q.save_json(root / "commands" / f'{artifacts["row_ordinal"]:04d}' / "native-observation.json", artifacts)
    budget.check()


def capture(args):
    with OwnedRoot(args.artifact_dir) as owner:
        return capture_owned(args, owner)


def capture_owned(args, owner):
    sock = socket.socket()
    owner.sock = sock
    sock.bind(("127.0.0.1", 0))
    sock.listen(8)
    try:
        root, frozen = freeze_inputs(args, sock, owner)
    except BaseException:
        sock.close()
        raise
    runner = runner_class()(root, frozen, args.timeout)
    owner.runner = runner
    runner.report.update(schema=SCHEMA, native_acceptance=False,
                         paired_acceptance=False, deferred_lanes=frozen["deferred_lanes"])
    by_name = {case.name: case for case in Q.cases()}
    records, errors = {}, {}
    try:
        for spec in frozen["plan"]:
            phase, name = spec["phase"], spec["case"]
            if phase == "server-start":
                runner.start_server(spec, sock)
            elif spec["row_type"] == "command":
                row = runner.command(spec)
                if phase == "version":
                    Q.equal([row["status"], Q.unblob(row["stdout"]).hex(), Q.unblob(row["stderr"]).hex()],
                            [0, NATIVE_VERSION.hex(), ""], "frozen native version")
                else:
                    case = by_name[name]
                    row["artifacts"] = observe(root, case, budget_for(frozen, snapshot=True,
                        deadline=runner.started + frozen["aggregate_timeout_seconds"]))
                    row["artifacts"]["row_ordinal"] = spec["ordinal"]
                    retain_cache(root, case, phase, row["artifacts"], budget_for(frozen,
                        deadline=runner.started + frozen["aggregate_timeout_seconds"]))
                    runner.persist()  # preserve wrong actual bytes BEFORE any assertion
                    try:
                        row["validation"] = validate_stage(row, spec, case, frozen)
                    except RuntimeViolation as error:
                        row["runtime_violation"] = str(error)
                        runner.persist()
                        raise
                    records.setdefault(name, {})[phase] = row
                    errors.setdefault(name, []).extend(phase + ": " + issue for issue in row["validation"]["issues"])
                runner.persist()
            elif phase == "case-end":
                try:
                    validate_case(by_name[name], records[name])
                except ValueError as error:
                    raise RuntimeViolation("native cold/warm cache/publication: " + str(error)) from error
                result = {"name": name, "classification": "UNMATCHED" if errors[name] else "MATCHED", "errors": errors[name]}
                runner.report["cases"].append(result)
                runner.append(spec, result=result, checkpoint=runner.checkpoint())
            elif phase == "server-stop":
                runner.append(spec, server=runner.stop_server())
            elif phase == "cleanup":
                runner.checkpoint()
                if any(Q.process_group_exists(pid) for pid in runner.groups):
                    raise Q.HealthFailure("native owned process-group leak")
                clean = owner.cleanup()
                if clean["errors"] or not clean["scratch_absent"] or not clean["all_groups_absent"]:
                    raise Q.HealthFailure("native owned cleanup failed: " + str(clean))
                runner.report["cleanup"] = {"groups": runner.groups, "all_groups_absent": True, "scratch_absent": True}
                runner.append(spec, cleanup=runner.report["cleanup"])
            elif phase == "pins-end":
                remaining = {path: pin for path, pin in frozen["pins"].items() if not Path(path).is_relative_to(root / "scratch")}
                budget = budget_for(frozen, deadline=runner.started + frozen["aggregate_timeout_seconds"])
                Q.equal({path: budget.pin(path) for path in remaining}, remaining, "native final runtime/QA pins")
                runner.append(spec, pins_sha256=Q.digest(Q.canonical(remaining)))
            else:
                runner.append(spec, checkpoint=runner.checkpoint())
        runner.report["state"] = "NATIVE_DIAGNOSTIC_CAPTURE"
    except RuntimeViolation as error:
        runner.report.update(state="NATIVE_RUNTIME_BLOCKER", blocker=str(error))
    except (Exception, KeyboardInterrupt) as error:
        runner.report.update(state="NATIVE_HEALTH_BLOCKER", blocker=type(error).__name__ + ": " + str(error))
    finally:
        sock.close()
        if runner.server is not None and not runner.server_stopped:
            try:
                runner.stop_server()
            except Exception as error:
                runner.report["cleanup_error"] = str(error)
                Q.kill_group(runner.server)
        cleanup = owner.cleanup()
        if cleanup["errors"] or not cleanup["scratch_absent"] or not cleanup["all_groups_absent"]:
            runner.report.update(state="NATIVE_HEALTH_BLOCKER", cleanup_error=cleanup)
        runner.report["cleanup"] = {"groups": runner.groups,
            "all_groups_absent": cleanup["all_groups_absent"], "scratch_absent": cleanup["scratch_absent"]}
        completed = {case["name"] for case in runner.report["cases"]}
        runner.report["uncompleted_cases"] = [case.name for case in Q.cases() if case.name not in completed]
        runner.report["counts"] = {"planned_cases": len(by_name), "captured_cases": len(completed),
            "matched": sum(c["classification"] == "MATCHED" for c in runner.report["cases"]),
            "unmatched": sum(c["classification"] == "UNMATCHED" for c in runner.report["cases"]),
            "planned_rows": len(frozen["plan"]), "actual_rows": len(runner.report["rows"]),
            "planned_commands": sum(r["row_type"] == "command" for r in frozen["plan"]),
            "actual_commands": sum(r["spec"]["row_type"] == "command" for r in runner.report["rows"]),
            "http_requests": len(runner.requests()), "deferred_not_executed": len(frozen["deferred_lanes"])}
        try:
            final_budget = budget_for(frozen, deadline=runner.started + frozen["aggregate_timeout_seconds"])
            runtime_binding(args.runtime_manifest, args.runtime_manifest_sha256, args.native, args.native_sha256, Q.ROOT, final_budget)
            if runner.report["state"] == "NATIVE_DIAGNOSTIC_CAPTURE":
                bind_retained(root, runner.report, frozen, final_budget)
        except Exception as error:
            runner.report.update(state="NATIVE_HEALTH_BLOCKER", blocker="final binding: " + str(error))
        runner.persist()
        seal_deadline = (runner.started + frozen["aggregate_timeout_seconds"] if runner.report["state"] == "NATIVE_DIAGNOSTIC_CAPTURE"
                         else time.monotonic() + frozen["cleanup_seconds"])
        Q.save_json(root / "SHA256SUMS.json", budget_for(frozen, deadline=seal_deadline).inventory(root))
    return runner.report


def bind_retained(root, report, frozen, budget=None):
    budget = budget or budget_for(frozen)
    original = Path(frozen["plan"][0]["cwd"]).parent
    requests = []
    for index, row in enumerate(report["rows"]):
        budget.check()
        spec = frozen["plan"][index]
        Q.equal(row["spec"], spec, "native retained chronology")
        if spec["phase"] == "server-stop":
            for stream in ("stdout", "stderr"):
                Q.equal(Q.blob(budget.read(root / ("server." + stream + ".raw"), boundary=root, limit=FS_LIMITS["artifact_file_bytes"])[0]), row["server"][stream], "native retained server stream")
        if spec["row_type"] != "command" or spec["phase"] == "server-start":
            continue
        directory = Path("commands") / f'{spec["ordinal"]:04d}'
        Q.equal(row["raw_directory"], str(original / directory), "native raw path provenance")
        for stream in ("stdout", "stderr"):
            Q.equal(Q.blob(budget.read(root / directory / (stream + ".raw"), boundary=root, limit=FS_LIMITS["artifact_file_bytes"])[0]), row[stream], "native retained raw bytes")
        requests.extend(row["requests"])
        if spec["case"]:
            observation = Q.load_json(budget.read(root / directory / "native-observation.json", boundary=root)[0])
            Q.equal(observation, row["artifacts"], "native independent observation bytes")
            retained = root / "native-cache" / spec["case"] / spec["phase"]
            actual = snapshot_tree(retained, budget, missing=True)
            for key in ("cache_exists", "files", "directories"):
                Q.equal(actual[key], row["artifacts"][key], "native independently retained " + key)
            # Retained copies are independent files; recorded live link groups
            # are evidence bound to those retained bytes by link_groups().
            Q.equal(actual["links"], [], "native retained copies are single-link files")
            link_groups(row["artifacts"])
            cache = original / "scratch/cache" / spec["case"]
            Q.equal(row["artifacts"]["cache_root"], str(cache), "retained actual cache boundary")
            verify_publication_discovery(row["artifacts"], actual, cache)
            # Manifests are bound to their actual cache files, not record-only objects.
            for item in row["artifacts"]["repositories"]:
                relative = str(Path(item["manifest_path"]).relative_to(cache))
                Q.inventory_key(relative)
                Q.equal(actual["files"][relative]["data"], item["manifest"], "native repository manifest binding")
                entries = snapshot_entries(actual, "repositories/direct/trees/" + item["identity"])
                Q.equal(entries, item["actual_entries"], "retained bytes/modes vs recorded repository entries")
                Q.equal(Q.load_json(bounded_blob(item["manifest"]))["files"], entries, "retained bytes/modes vs repository manifest")
            for item in row["artifacts"]["actions"]:
                relative = str(Path(item["path"]).relative_to(cache))
                Q.inventory_key(relative)
                Q.equal(actual["files"][relative]["data"], item["manifest"], "native action manifest binding")
    path = root / "requests.jsonl"
    try:
        request_bytes = budget.read(path, boundary=root, limit=FS_LIMITS["artifact_file_bytes"])[0]
    except FileNotFoundError:
        request_bytes = b""
    actual = []
    for line in request_bytes.split(b"\n"):
        budget.check()
        if line:
            actual.append(Q.load_json(line))
            require(len(actual) <= 512, "retained request count limit")
    Q.equal(actual, requests, "native complete all-method request ledger")
    for case, files in frozen["fixtures"].items():
        for name, value in files.items():
            Q.inventory_key(case)
            Q.inventory_key(name)
            Q.equal(Q.blob(budget.read(root / "inputs/workspaces" / case / name, boundary=root)[0]), value, "native frozen fixture snapshot")
    budget.check()


def assess(report, frozen, preparation=False, budget=None):
    budget = budget or budget_for(frozen)
    Q.equal(report["state"], "NATIVE_DIAGNOSTIC_CAPTURE", "native capture incomplete or blocked")
    Q.equal([frozen["schema"], frozen["mode"]], [SCHEMA, "native-diagnostic"], "native diagnostic schema")
    Q.equal([report["schema"], report["scope"], report["native_acceptance"], report["paired_acceptance"]],
            [frozen["schema"], frozen["scope"], False, False], "native diagnostic acceptance scope")
    Q.equal(report["uncompleted_cases"], [], "native case coverage has no skipped suffix")
    validate_runtime_generation(frozen["runtime_binding"])
    Q.equal(frozen["source_binding_limits"], SOURCE_BINDING_LIMITS, "source-binding budget generation")
    Q.equal(frozen["case_definitions"], [Q.asdict(c) for c in Q.cases()], "native exact case recipes")
    Q.equal(frozen["deferred_lanes"], deferred_lanes(), "native deferred coverage not passed")
    root = Path(frozen["plan"][0]["cwd"]).parent
    expected_plan = plan(root, frozen["native"])
    expected_plan[4] = frozen["plan"][4]
    Q.equal(frozen["plan"], expected_plan, "native trusted full command plan")
    Q.equal(len(report["rows"]), len(expected_plan), "native complete chronology")
    by_name = {c.name: c for c in Q.cases()}
    grouped, issues, results, old_results, requests, pids = {}, {}, [], [], [], []
    legacy_header_rows = []
    ready = None
    for index, (row, spec) in enumerate(zip(report["rows"], frozen["plan"])):
        budget.check()
        Q.equal(spec["ordinal"], index, "native ordinal")
        Q.validate_lifecycle_row(row, spec, frozen, ready)
        phase, name = spec["phase"], spec["case"]
        if phase == "server-start":
            ready = row["ready"]
            pids.append(row["pid"])
        elif spec["row_type"] == "command":
            pids.append(row["pid"])
            health = Q.stage_health(row, frozen["responses"], frozen["origin"], allow_legacy_headers=preparation)
            if health["legacy_dict_only_requests"]:
                legacy_header_rows.append({"ordinal": index, "requests": health["legacy_dict_only_requests"]})
            requests.extend(row["requests"])
            if phase == "version":
                Q.equal([row["status"], Q.unblob(row["stdout"]).hex(), Q.unblob(row["stderr"]).hex()], [0, NATIVE_VERSION.hex(), ""], "native version")
            else:
                validation = validate_stage(row, spec, by_name[name], frozen)
                if not preparation:
                    Q.equal(row["validation"], validation, "native recorded checker verdict")
                grouped.setdefault(name, {})[phase] = {**row, "validation": validation}
                issues.setdefault(name, []).extend(phase + ": " + issue for issue in validation["issues"])
                Q.equal([row["artifacts"]["callback_scratch_absent"], row["artifacts"]["action_work_absent"]], [True, True], "native lifetime/work cleanup")
        elif phase == "case-end":
            validate_case(by_name[name], grouped[name])
            result = {"name": name, "classification": "UNMATCHED" if issues[name] else "MATCHED", "errors": issues[name]}
            if not preparation:
                Q.equal(row["result"], result, "native case verdict")
            if row["result"]["classification"] not in ("MATCHED", "UNMATCHED"):
                raise ValueError("native skipped/unknown classification")
            Q.equal(row["result"]["name"], name, "native historical case identity")
            old_results.append(row["result"])
            results.append(result)
    Q.equal(report["cases"], old_results if preparation else results, "native case inventory")
    Q.equal(report["cleanup"], {"groups": pids, "all_groups_absent": True, "scratch_absent": True}, "native resource cleanup")
    Q.equal([r["ordinal"] for r in requests], list(range(len(requests))), "native global method/request chronology")
    counts = {"planned_cases": len(by_name), "captured_cases": len(results),
              "matched": sum(c["classification"] == "MATCHED" for c in results),
              "unmatched": sum(c["classification"] == "UNMATCHED" for c in results),
              "planned_rows": len(expected_plan), "actual_rows": len(report["rows"]),
              "planned_commands": sum(r["row_type"] == "command" for r in expected_plan),
              "actual_commands": len(pids), "http_requests": len(requests), "deferred_not_executed": len(deferred_lanes())}
    old_counts = {**counts, "matched": sum(r["classification"] == "MATCHED" for r in old_results),
                  "unmatched": sum(r["classification"] == "UNMATCHED" for r in old_results)}
    Q.equal(report["counts"], old_counts if preparation else counts, "native actual counts")
    budget.check()
    return {"state": "NATIVE_PREPARATION_REPLAY_ONLY" if preparation else "NATIVE_DIAGNOSTIC_REPLAYED",
            "counts": counts, "historical_counts": old_counts, "cases": results,
            "native_acceptance": False, "paired_acceptance": False,
            "historical_header_limitations": {"dict_only_request_rows": legacy_header_rows,
                "note": "Dictionary-only headers are limited preparation evidence, not full capture proof."}}


def replay(args, preparation=False):
    root = Path(args.artifact_dir).absolute()
    budget = budget_for()
    inventory = budget.inventory(root)
    manifest_raw = budget.read(root / "SHA256SUMS.json", boundary=root)[0]
    Q.equal(Q.digest(manifest_raw), args.archive_sha256, "external native archive pin")
    Q.equal(Q.inventory_map(Q.load_json(manifest_raw)), inventory, "bounded exact native archive inventory")
    freeze_raw = budget.read(root / "freeze.json", boundary=root)[0]
    Q.equal(Q.digest(freeze_raw), args.freeze_sha256, "external native freeze pin")
    frozen = Q.load_json(freeze_raw)
    Q.equal(frozen["fs_limits"], FS_LIMITS, "native replay frozen filesystem limits")
    report_raw = budget.read(root / "report.json", boundary=root)[0]
    report = Q.load_json(report_raw)
    Q.equal(report["freeze_sha256"], args.freeze_sha256, "native report/freeze pin")
    _, _, current = runtime_binding(args.runtime_manifest, args.runtime_manifest_sha256, args.native_path,
                                   args.native_sha256, Q.ROOT, budget)
    Q.equal(current, frozen["runtime_binding"], "current frozen runtime binding")
    current_qa = {name: budget.pin(Q.ROOT / Q.inventory_key(name)) for name in frozen["qa_sources"]}
    if not preparation:
        Q.equal(current_qa, frozen["qa_sources"], "current native QA binding")
    bind_retained(root, report, frozen, budget)
    result = assess(report, frozen, preparation, budget)
    Q.equal({name: budget.pin(Q.ROOT / name) for name in frozen["qa_sources"]}, current_qa, "native replay QA drift")
    _, _, after = runtime_binding(args.runtime_manifest, args.runtime_manifest_sha256, args.native_path,
                                 args.native_sha256, Q.ROOT, budget)
    Q.equal(after, current, "native replay runtime drift")
    result.update(runtime_binding=current, current_qa_sources=current_qa,
                  historical_qa_sources=frozen["qa_sources"], historical_report_sha256=Q.digest(report_raw))
    budget.check()
    return result


def compare_pair(nr, nf, rr, rf, native_result, ref_result):
    validate_runtime_generation(nf.get("runtime_binding"))
    Q.equal(native_result.get("runtime_binding"), nf["runtime_binding"], "paired native frozen generation")
    Q.equal(nf["case_definitions"], rf["case_definitions"], "paired exact recipes")
    for result in (native_result, ref_result):
        Q.equal([result["counts"]["captured_cases"], result["counts"]["matched"], result["counts"]["unmatched"]], [33, 33, 0], "paired complete matched coverage")
    # A matching *current* server/collector source is necessary: historical
    # GET-only method ledgers cannot silently become paired transport proof.
    for name, pin in nf["qa_sources"].items():
        Q.equal(rf["pins"].get(str(Q.ROOT / name)), pin, "paired same reviewed QA source")
    for case in Q.cases():
        n = next(r for r in nr["rows"] if r["spec"]["case"] == case.name and r["spec"]["phase"] in ("cold", "negative"))
        r = next(r for r in rr["rows"] if r["spec"]["case"] == case.name and r["spec"]["phase"] in ("cold", "negative"))
        Q.equal(n["validation"]["events"], r["validation"]["events"], "paired logical observations, not raw UTF8/Latin1 equality")
        Q.equal([x["path"] for x in n["requests"]], [x["path"] for x in r["requests"]], "paired ordered HTTP effects")
    return {"state": "PAIRED_REPLAY_CRITERIA_MATCHED_PENDING_PARENT_ALL_SOURCE_ACCEPTANCE",
            "cases": 33, "deferred_not_executed": len(deferred_lanes()), "native_acceptance": False,
            "full_parity": False, "application_success": False}


def paired_replay(args):
    """No paired acceptance from a stale reference, deferred lane or native diagnostic."""
    native_result = replay(args)
    reference = Path(args.reference_artifact).absolute()
    ref_result = Q.replay_reference(reference, args.reference_freeze_sha256, args.reference_archive_sha256, args.reference)
    budget = budget_for()
    nf = Q.load_json(budget.read(Path(args.artifact_dir) / "freeze.json")[0])
    nr = Q.load_json(budget.read(Path(args.artifact_dir) / "report.json")[0])
    rf = Q.load_json(budget.read(reference / "freeze.json")[0])
    rr = Q.load_json(budget.read(reference / "report.json")[0])
    result = compare_pair(nr, nf, rr, rf, native_result, ref_result)
    budget.check()
    return result
