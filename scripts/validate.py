#!/usr/bin/env python3
"""Scoped Bazel 9.3/native differential checks; not a full compatibility suite.

Uses only the Python standard library. Run from any directory; executable defaults
are relative to this checkout. Reports survive cleanup of private build state.
No Bazel test command is run and no repository wrapper or direnv is invoked.
"""

import argparse
import base64
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
EXPECTED_UPPER = b"HELLO FROM NATIVE RUST\n"
REPO_TARGET = "//src/docker:config"
UNSUPPORTED_TARGET = "//src/beyondwork.ai/utils/str:all"
LABEL = re.compile(r"//[^\s:]*:[^\s:]+\Z")
COMMON_FLAGS = (
    "--lockfile_mode=off",
    "--repo_env=DO_NOT_TRACK=1",
    "--remote_cache=",
    "--remote_executor=",
    "--bes_backend=",
    "--remote_upload_local_results=false",
    "--color=no",
    "--curses=no",
)
BUILD_FLAGS = (
    "--experimental_convenience_symlinks=ignore",
    "--jobs=2",
)


def positive_int(value):
    try:
        result = int(value)
    except (TypeError, ValueError) as exc:
        raise argparse.ArgumentTypeError("must be a positive integer") from exc
    if result <= 0:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return result


def positive_seconds(value):
    try:
        result = float(value)
    except (TypeError, ValueError) as exc:
        raise argparse.ArgumentTypeError("must be positive finite seconds") from exc
    if not math.isfinite(result) or result <= 0:
        raise argparse.ArgumentTypeError("must be positive finite seconds")
    return result


def percentile(samples, percent):
    """Linearly interpolated percentile, rank (n - 1) * percent / 100."""
    values = sorted(samples)
    if not values or not 0 <= percent <= 100:
        raise ValueError("percentile requires samples and a percent in [0, 100]")
    if any(not math.isfinite(x) or x < 0 for x in values):
        raise ValueError("timings must be finite and nonnegative")
    rank = (len(values) - 1) * percent / 100
    low, high = math.floor(rank), math.ceil(rank)
    return values[low] + (values[high] - values[low]) * (rank - low)


def summarize(samples):
    values = list(samples)
    p95 = percentile(values, 95)
    return {
        "count": len(values), "median_seconds": statistics.median(values),
        "p95_seconds": p95, "min_seconds": min(values),
        "max_seconds": max(values), "samples_seconds": values,
        "percentile_method": "linear interpolation at (n - 1) * p / 100",
    }


def parse_labels(stdout):
    labels = [line.strip() for line in stdout.splitlines() if line.strip()]
    if any(not LABEL.fullmatch(label) for label in labels):
        raise ValueError("query stdout contains a non-label line")
    if len(labels) != len(set(labels)):
        raise ValueError("query stdout contains duplicate labels")
    return sorted(labels)


def parse_native_outputs(stdout):
    """Multiple files for the same filegroup label are legitimate."""
    outputs = []
    for line in stdout.splitlines():
        if not line:
            continue
        parts = line.split("\t")
        if len(parts) != 2 or not LABEL.fullmatch(parts[0]):
            raise ValueError("expected LABEL<TAB>ABSOLUTE_OUTPUT_PATH")
        path = Path(parts[1])
        if not path.is_absolute():
            raise ValueError("native output path must be absolute")
        item = (parts[0], path)
        if item in outputs:
            raise ValueError("duplicate native output")
        outputs.append(item)
    return outputs


def reference_output_paths(stdout, workspace, output_base):
    """cquery --output=files paths are execution-root relative, not symlinks."""
    outputs = []
    for line in stdout.splitlines():
        if not line:
            continue
        path = Path(line)
        if path.is_absolute() or ".." in path.parts or not path.parts:
            raise ValueError("unsafe cquery output path: " + line)
        root = (output_base / "execroot" / "_main"
                if path.parts[0] in ("bazel-out", "external") else workspace)
        outputs.append(root / path)
    if len(outputs) != len(set(outputs)):
        raise ValueError("duplicate cquery output")
    return outputs


def clean_environment():
    env = dict(os.environ)
    for name in ("BAZELRC", "USE_BAZEL_VERSION", "WORKSPACE_ROOT"):
        env.pop(name, None)
    env["DO_NOT_TRACK"] = "1"
    env["BAZEL_METRICS"] = "0"
    env["NEXT_TELEMETRY_DISABLED"] = "1"
    return env


def kill_process_group(pid):
    """Only used for a child created with start_new_session=True."""
    try:
        os.killpg(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


class Runner:
    def __init__(self, report, timeout):
        self.report = report
        self.timeout = timeout
        self.env = clean_environment()

    def run(self, name, argv, cwd, timeout=None):
        record = {
            "id": len(self.report["runs"]), "name": name,
            "argv": [str(arg) for arg in argv], "cwd": str(cwd),
            "timeout_seconds": self.timeout if timeout is None else timeout,
            "elapsed_seconds": None, "returncode": None,
            "stdout": "", "stderr": "", "timed_out": False,
        }
        self.report["runs"].append(record)
        start = time.perf_counter()
        process = None
        stdout, stderr = b"", b""
        try:
            process = subprocess.Popen(
                record["argv"], cwd=str(cwd), env=self.env,
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, start_new_session=True,
            )
            try:
                stdout, stderr = process.communicate(timeout=record["timeout_seconds"])
            except subprocess.TimeoutExpired:
                record["timed_out"] = True
                # Kill the group even if its leader has already exited: a
                # descendant may still hold the stdout/stderr pipe open.
                kill_process_group(process.pid)
                stdout, stderr = process.communicate()
            except BaseException:
                record["interrupted"] = True
                kill_process_group(process.pid)
                stdout, stderr = process.communicate()
                raise
        except OSError as exc:
            record["spawn_error"] = str(exc)
        finally:
            record["elapsed_seconds"] = time.perf_counter() - start
            record["returncode"] = process.returncode if process is not None else None
            # Preserve exact bytes too if the program printed non-UTF-8 data.
            for field, data in (("stdout", stdout), ("stderr", stderr)):
                record[field] = data.decode("utf-8", errors="replace")
                if record[field].encode("utf-8") != data:
                    record[field + "_base64"] = base64.b64encode(data).decode("ascii")
        return record


def require_success(record):
    if record["returncode"] != 0 or record["timed_out"]:
        raise ValueError("invocation {} ({}) failed; see runs[{}]".format(
            record["name"], record["returncode"], record["id"]))
    return record["stdout"]


class Reference:
    def __init__(self, executable, workspace, scratch, runner, name):
        self.workspace, self.runner, self.name = workspace, runner, name
        self.output_base = scratch / (name + "-output-base")
        self.output_user_root = scratch / (name + "-output-user-root")
        self.startup = [
            str(executable), "--ignore_all_rc_files",
            "--output_user_root=" + str(self.output_user_root),
            "--output_base=" + str(self.output_base),
            "--host_jvm_args=-Xmx512m", "--host_jvm_args=-XX:ActiveProcessorCount=2",
            "--max_idle_secs=30",
        ]
        self.started = False

    def run(self, name, command, *args):
        flags = list(COMMON_FLAGS)
        if command in ("build", "cquery"):
            flags.extend(BUILD_FLAGS)
        self.started = True
        return self.runner.run(name, self.startup + [command] + flags + list(args), self.workspace)

    def shutdown(self):
        if not self.started:
            return
        result = self.runner.run(self.name + ".shutdown", self.startup + ["shutdown"] + list(COMMON_FLAGS),
                                 self.workspace, timeout=min(self.runner.timeout, 15))
        if result["returncode"] == 0 and not result["timed_out"]:
            return
        # Bazel daemonizes separately from the timed-out client. A fallback is
        # allowed ONLY for the PID in our private output base, with its exact
        # output-base argument independently confirmed from /proc. Never pkill.
        cleanup = {"output_base": str(self.output_base), "forced": False}
        self.runner.report["cleanup"].append(cleanup)
        try:
            pid = int((self.output_base / "server" / "server.pid.txt").read_text().strip())
            if pid <= 1:
                raise ValueError("invalid server pid")
            arguments = Path("/proc", str(pid), "cmdline").read_bytes().split(b"\0")
            expected = os.fsencode("--output_base=" + str(self.output_base))
            if expected not in arguments:
                raise ValueError("refusing to kill an unverified server pid")
            if os.getpgid(pid) == pid:
                kill_process_group(pid)
            else:
                os.kill(pid, signal.SIGKILL)
            cleanup.update(forced=True, pid=pid)
        except (FileNotFoundError, ProcessLookupError):
            cleanup["already_exited"] = True
        except (OSError, ValueError) as exc:
            cleanup["error"] = str(exc)
            self.runner.report["errors"].append("server cleanup: " + str(exc))


def native_command(executable, cache, command, target):
    return [str(executable), "--ignore_all_rc_files", "--cache-dir=" + str(cache), command, target]


def check(report, name, callback):
    result = {"name": name, "status": "FAIL"}
    report["checks"].append(result)
    try:
        result["details"] = callback()
        result["status"] = "PASS"
    except (OSError, ValueError) as exc:
        result["error"] = str(exc)
    return result


def query_benchmark(report, name, invoke, expected, iterations):
    benchmark = {"pattern": "//:all", "warm": None, "run_ids": []}
    report["benchmarks"][name] = benchmark
    first_start = len(report["runs"])
    first = invoke(name + ".query.first")
    attempts = report["runs"][first_start:]
    benchmark["first_invocation"] = {
        "run_id": attempts[0]["id"], "seconds": attempts[0]["elapsed_seconds"],
        "returncode": attempts[0]["returncode"],
        "query_ready_seconds_including_flag_retry": sum(r["elapsed_seconds"] for r in attempts),
        "flag_retry": len(attempts) > 1,
    }
    if parse_labels(require_success(first)) != expected:
        raise ValueError(name + " first query labels do not match expected fixture labels")
    samples = []
    for index in range(iterations):
        result = invoke(name + ".query.warm." + str(index + 1))
        benchmark["run_ids"].append(result["id"])
        if parse_labels(require_success(result)) != expected:
            raise ValueError(name + " warmed query labels changed")
        samples.append(result["elapsed_seconds"])
    benchmark["warm"] = summarize(samples)
    return expected


def file_evidence(path, expected=None, include_bytes=False):
    data = path.read_bytes()
    if expected is not None and data != expected:
        raise ValueError("unexpected output bytes: " + str(path))
    evidence = {"path": str(path), "size_bytes": len(data),
                "sha256": hashlib.sha256(data).hexdigest()}
    if include_bytes:
        evidence["bytes_base64"] = base64.b64encode(data).decode("ascii")
    return evidence


def native_paths(result, target):
    outputs = parse_native_outputs(require_success(result))
    if any(label != target for label, _ in outputs):
        raise ValueError("native build returned an unexpected target label")
    return [path for _, path in outputs]


def fixture_builds(native, reference, cache, runner):
    details = {"native": [], "reference": []}
    reference_paths = None
    for implementation in ("native", "reference"):
        for repetition in (1, 2):
            name = "fixture.{}.build.{}".format(implementation, repetition)
            if implementation == "native":
                result = runner.run(name, native_command(native, cache, "build", "//:upper"),
                                    reference.workspace)
                paths = native_paths(result, "//:upper")
            else:
                result = reference.run(name, "build", "//:upper")
                require_success(result)
                if reference_paths is None:
                    listing = reference.run("fixture.reference.output-files", "cquery",
                                            "--output=files", "//:upper")
                    reference_paths = reference_output_paths(
                        require_success(listing), reference.workspace, reference.output_base)
                paths = reference_paths
            if len(paths) != 1:
                raise ValueError("//:upper must produce exactly one output")
            evidence = file_evidence(paths[0], EXPECTED_UPPER, include_bytes=True)
            evidence.update(run_id=result["id"], scenario="first build" if repetition == 1 else "unchanged rebuild")
            if implementation == "native":
                match = re.search(r"(\d+) actions executed, (\d+) cache hits", result["stderr"])
                if not match:
                    raise ValueError("native build did not report action/cache counts")
                evidence["actions_executed"], evidence["cache_hits"] = map(int, match.groups())
                expected_counts = (2, 0) if repetition == 1 else (0, 2)
                if (evidence["actions_executed"], evidence["cache_hits"]) != expected_counts:
                    raise ValueError("native fixture did not execute/cache the two expected actions")
            details[implementation].append(evidence)
    return details


def repo_parity(native, reference, cache, runner):
    queries = {}
    for implementation in ("native", "reference"):
        name = "repo." + implementation + ".query"
        result = (runner.run(name, native_command(native, cache, "query", REPO_TARGET), reference.workspace)
                  if implementation == "native" else reference.run(name, "query", REPO_TARGET))
        queries[implementation] = parse_labels(require_success(result))
    if queries["native"] != queries["reference"] or queries["native"] != [REPO_TARGET]:
        raise ValueError("repository filegroup query labels differ")
    native_result = runner.run("repo.native.build", native_command(native, cache, "build", REPO_TARGET),
                               reference.workspace)
    native_files = native_paths(native_result, REPO_TARGET)
    expected_path = reference.workspace / "src" / "docker" / "config.json"
    if native_files != [expected_path]:
        raise ValueError("native config filegroup no longer contains exactly src/docker/config.json")
    data = native_files[0].read_bytes()
    native_evidence = file_evidence(native_files[0], data)
    require_success(reference.run("repo.reference.build", "build", REPO_TARGET))
    listing = reference.run("repo.reference.output-files", "cquery", "--output=files", REPO_TARGET)
    reference_files = reference_output_paths(require_success(listing), reference.workspace, reference.output_base)
    # This deliberately checks only the documented source-only filegroup; it
    # must not quietly turn into a request to compile a larger repository target.
    if reference_files != [expected_path]:
        raise ValueError("config filegroup no longer contains exactly src/docker/config.json")
    return {"labels": queries, "native": [native_evidence],
            "reference": [file_evidence(reference_files[0], data)]}


def unsupported_probe(native, repo, cache, runner):
    result = runner.run("repo.native.expected-unsupported",
                        native_command(native, cache, "query", UNSUPPORTED_TARGET), repo)
    expected = (result["returncode"] is not None and result["returncode"] > 0
                and not result["timed_out"] and not result["stdout"].strip()
                and any(word in result["stderr"].lower() for word in ("load", "external repository"))
                and any(word in result["stderr"].lower() for word in ("unsupported", "not supported")))
    item = {"target": UNSUPPORTED_TARGET, "command": "query", "run_id": result["id"],
            "status": "EXPECTED_FAILURE" if expected else "UNEXPECTED_RESULT",
             "reason": "External repository resolution / rules_go remain unsupported; not a parity pass or a test run."}
    runner.report["unsupported"].append(item)
    if not expected:
        runner.report["errors"].append("unsupported query did not produce the expected load rejection")


def directory_size(path):
    total = 0
    for directory, _, files in os.walk(path, followlinks=False):
        for name in files:
            entry = Path(directory) / name
            try:
                if not entry.is_symlink():
                    total += entry.stat().st_size
            except FileNotFoundError:
                pass
    return total


def validate_executable(path, official=False):
    if not path.is_file() or not os.access(path, os.X_OK):
        raise ValueError("executable missing or not executable: " + str(path))
    if official:
        with path.open("rb") as stream:
            if stream.read(4) != b"\x7fELF":
                raise ValueError("--reference must be the official Linux ELF binary, not a wrapper")


def write_report(path, report):
    # Write beside the destination then replace, keeping partial writes out of
    # the final artifact and never writing the report inside the scratch tree.
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent,
                                     prefix=path.name + ".", suffix=".tmp", delete=False) as stream:
        temporary = Path(stream.name)
        try:
            json.dump(report, stream, indent=2, sort_keys=True, allow_nan=False)
            stream.write("\n")
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    try:
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=ROOT / "target/release/bazel-rs")
    parser.add_argument("--reference", type=Path,
                        default=ROOT / ".direnv/reference/bazel-9.3.0-linux-x86_64")
    parser.add_argument("--repo", type=Path, default=Path.home() / "code")
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/validation.json")
    parser.add_argument("--iterations", type=positive_int, default=10,
                        help="warmed query samples per executable (first invocation is additional)")
    parser.add_argument("--timeout", type=positive_seconds, default=90,
                        help="timeout per invocation in seconds")
    parser.add_argument("--skip-repo", action="store_true")
    return parser


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ("native", "reference", "repo", "output"):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {
        "schema_version": 1, "status": "FAIL", "runs": [], "checks": [],
        "benchmarks": {}, "unsupported": [], "errors": [], "cleanup": [],
        "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "configuration": {key: str(value) if isinstance(value, Path) else value
                          for key, value in vars(args).items()},
        "reference_version_required": "9.3.0", "repo_version_pin_expected": "9.1.1",
        "scope": ["fixture query labels", "fixture genrule output bytes twice"]
                 + ([] if args.skip_repo else ["real repository source-only config filegroup"]),
        "test_commands_executed": False,
        "environment_policy": {"unset": ["BAZELRC", "USE_BAZEL_VERSION", "WORKSPACE_ROOT"],
                               "DO_NOT_TRACK": "1", "BAZEL_METRICS": "0",
                               "NEXT_TELEMETRY_DISABLED": "1", "repo_env": {"DO_NOT_TRACK": "1"}},
        "cache_scenario": {
            "initial": "unique empty output roots/native caches; OS page cache NOT flushed",
            "warm": "same persistent official server/output base; native fresh process each time",
            "builds": "first build and unchanged rebuild; no source edits or forced invalidation",
        },
        "limitations": [
            "PASS covers only the explicitly listed scope, not full Bazel compatibility.",
            "Repository pins 9.1.1; the reference is explicitly 9.3.0, without repository rc configuration.",
            "Only query and build are compared; no Bazel tests or heavy-language targets run.",
            "Native execution is not sandboxed and has no upstream fallback.",
            "Isolated first invocation includes server/JVM/install startup, not OS-cold performance.",
            "Remote cache/executor/BES are disabled; repository/module downloads may still require network.",
            "DO_NOT_TRACK is set; this is not a network sandbox for arbitrary repository rules.",
            "Query samples are grouped by implementation, not interleaved; these are exploratory timings.",
        ],
    }
    runner = Runner(report, args.timeout)
    references = []
    scratch = None
    interrupted = False
    try:
        validate_executable(args.native)
        validate_executable(args.reference, official=True)
        report["executables"] = {
            "native": file_evidence(args.native),
            "reference": file_evidence(args.reference),
        }
        temporary_parent = Path("/tmp/opencode")
        if not temporary_parent.is_dir():
            temporary_parent = args.output.parent
        scratch = Path(tempfile.mkdtemp(prefix="bazel-validation-", dir=temporary_parent))
        report["scratch_directory"] = str(scratch)
        fixture = scratch / "fixture"
        shutil.copytree(ROOT / "tests/fixtures/basic", fixture)
        ref = Reference(args.reference, fixture, scratch, runner, "fixture")
        references.append(ref)
        native_cache = scratch / "fixture-native-cache"
        labels = ["//:copy", "//:inputs", "//:upper"]
        check(report, "fixture.native.query", lambda: query_benchmark(
            report, "fixture.native", lambda name: runner.run(
                name, native_command(args.native, native_cache, "query", "//:all"), fixture),
            labels, args.iterations))
        check(report, "fixture.reference.query", lambda: query_benchmark(
            report, "fixture.reference", lambda name: ref.run(name, "query", "//:all"),
            labels, args.iterations))
        release = ref.run("reference.release", "info", "release")
        report["reference_release"] = require_success(release).strip()
        if report["reference_release"] != "release 9.3.0":
            raise ValueError("reference binary is not official release 9.3.0")
        check(report, "fixture.build-output-parity", lambda: fixture_builds(
            args.native, ref, native_cache, runner))
        if args.skip_repo:
            report["unsupported"].append({"target": UNSUPPORTED_TARGET, "status": "NOT_RUN",
                                          "reason": "--skip-repo"})
            report["limitations"].append("Real repository parity and unsupported-load probe were skipped.")
        else:
            if not (args.repo / "MODULE.bazel").is_file():
                raise ValueError("--repo is not a module workspace: " + str(args.repo))
            pin = args.repo / ".bazelversion"
            report["repo_version_pin_observed"] = pin.read_text().strip() if pin.is_file() else None
            if report["repo_version_pin_observed"] is None:
                bazeliskrc = args.repo / ".bazeliskrc"
                if bazeliskrc.is_file():
                    match = re.search(r"^USE_BAZEL_VERSION=(.+)$", bazeliskrc.read_text(), re.M)
                    if match:
                        report["repo_version_pin_observed"] = match.group(1).strip()
                        report["repo_version_pin_source"] = str(bazeliskrc)
            repo_ref = Reference(args.reference, args.repo, scratch, runner, "repo")
            references.append(repo_ref)
            repo_cache = scratch / "repo-native-cache"
            check(report, "repo.config-filegroup-parity", lambda: repo_parity(
                args.native, repo_ref, repo_cache, runner))
            unsupported_probe(args.native, args.repo, repo_cache, runner)
    except KeyboardInterrupt:
        interrupted = True
        report["errors"].append("interrupted")
    except (OSError, ValueError) as exc:
        report["errors"].append(str(exc))
    finally:
        for reference in reversed(references):
            try:
                reference.shutdown()
            except (OSError, ValueError) as exc:
                report["errors"].append("shutdown failed: " + str(exc))
        if scratch is not None:
            try:
                report["cache_sizes_bytes_before_cleanup"] = {
                    path.name: directory_size(path) for path in scratch.iterdir() if path.is_dir()
                }
            except OSError as exc:
                report["cache_size_error"] = str(exc)
            try:
                shutil.rmtree(scratch)
                report["scratch_cleaned"] = True
            except OSError as exc:
                report["scratch_cleaned"] = False
                report["errors"].append("scratch cleanup failed: " + str(exc))
        expected_checks = 3 if args.skip_repo else 4
        if (len(report["checks"]) == expected_checks and not report["errors"]
                and all(item["status"] == "PASS" for item in report["checks"])):
            report["status"] = "PASS"
        report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.output, report)
    print("{} (scoped parity only): {}".format(report["status"], args.output))
    return 130 if interrupted else (0 if report["status"] == "PASS" else 1)


if __name__ == "__main__":
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
