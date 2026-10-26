"""Capture genuine Bazel 9.3.0 `mod` for tests/mod_command.rs.

Scratch only: the fixture (registry/, sources/, ws/) is copied to a private directory under
/tmp/opencode; a private HOME and --output_user_root. Registries: the fixture's local registry
(relative module_base_path, local_path sources) first, then the public BCR for the genuine
built-in closure of @bazel_tools (genuine evaluates every module extension of every module for
any graph subcommand, which needs the built-in modules' sources; the offline
tests/fixtures/builtin_modules registry lacks two of their patches). The native test resolves
the same graph from the offline registry. Every case runs in --batch against ONE output base.
Run: python3 tests/fixtures/mod_command/capture.py (writes reference_receipt.json here).
"""
import hashlib
import json
import os
import re
import shutil
import subprocess
import time

GENUINE = "/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64"
SCRATCH = "/tmp/opencode/mod-command-capture"
FIXTURE = SCRATCH + "/fixture"
WS = FIXTURE + "/ws"
OUR = SCRATCH + "/our"
HOME = SCRATCH + "/home"
HERE = os.path.dirname(os.path.abspath(__file__))
BCR = "https://bcr.bazel.build"

EXT = "@mc_a//:ext.bzl%ext"
CASES = [
    # graph and its display options
    ("graph", ["graph"]),
    ("graph_ascii", ["graph", "--charset=ascii"]),
    ("graph_json", ["graph", "--output=json"]),
    ("graph_dot", ["graph", "--output=graph"]),
    ("graph_builtin", ["graph", "--include_builtin"]),
    ("graph_builtin_json", ["graph", "--include_builtin", "--output=json"]),
    ("graph_unused", ["graph", "--include_unused"]),
    ("graph_verbose", ["graph", "--verbose"]),
    ("graph_unused_verbose", ["graph", "--include_unused", "--verbose"]),
    ("graph_unused_verbose_json", ["graph", "--include_unused", "--verbose", "--output=json"]),
    ("graph_unused_verbose_dot", ["graph", "--include_unused", "--verbose", "--output=graph"]),
    ("graph_depth", ["graph", "--depth=2"]),
    ("graph_nocycles", ["graph", "--nocycles"]),
    ("graph_from", ["graph", "--from=mc_cyc_x"]),
    ("graph_from_two", ["graph", "--from=mc_cyc_y,mc_local"]),
    ("graph_ignore_dev", ["graph", "--ignore_dev_dependency"]),
    ("graph_ext_usages", ["graph", "--extension_info=usages"]),
    ("graph_ext_repos", ["graph", "--extension_info=repos"]),
    ("graph_ext_all", ["graph", "--extension_info=all"]),
    ("graph_ext_all_json", ["graph", "--extension_info=all", "--output=json"]),
    ("graph_ext_all_dot", ["graph", "--extension_info=all", "--output=graph"]),
    ("graph_ext_filter", ["graph", "--extension_filter=" + EXT]),
    ("graph_ext_filter_all", ["graph", "--extension_filter=" + EXT, "--extension_info=all"]),
    ("graph_ext_filter_empty", ["graph", "--extension_filter=", "--extension_info=repos"]),
    # deps / paths / explain
    ("deps", ["deps", "mc_b"]),
    ("deps_root", ["deps", "<root>"]),
    ("deps_depth", ["deps", "mc_a", "--depth=1"]),
    ("deps_apparent", ["deps", "@bee"]),
    ("deps_canonical", ["deps", "@@mc_local+"]),
    ("all_paths", ["all_paths", "mc_leaf"]),
    ("all_paths_from", ["all_paths", "mc_cyc_x", "--from=mc_a"]),
    ("path", ["path", "mc_leaf"]),
    ("path_json", ["path", "mc_leaf", "--output=json"]),
    ("path_none", ["path", "mc_a", "--from=mc_b"]),
    ("path_no_targets", ["path"]),
    ("explain", ["explain", "mc_leaf"]),
    ("explain_unused", ["explain", "mc_b", "--include_unused"]),
    ("explain_verbose", ["explain", "mc_svo", "--include_unused", "--verbose"]),
    ("explain_unused_version", ["explain", "mc_leaf@1.0"]),
    ("explain_unused_version_ok", ["explain", "mc_leaf@1.0", "--include_unused"]),
    # show_repo / show_extension
    ("show_repo_apparent", ["show_repo", "@mc_a"]),
    ("show_repo_module", ["show_repo", "mc_b"]),
    ("show_repo_canonical", ["show_repo", "@@mc_local+"]),
    ("show_repo_extension", ["show_repo", "@my_y"]),
    ("show_repo_direct", ["show_repo", "@direct"]),
    ("show_repo_builtin", ["show_repo", "@bazel_tools"]),
    ("show_repo_base_module", ["show_repo", "--base_module=mc_a", "@x_repo"]),
    ("show_repo_missing", ["show_repo", "@nope"]),
    ("show_repo_canonical_missing", ["show_repo", "@@nope+"]),
    ("show_repo_jsonproto", ["show_repo", "--output=streamed_jsonproto", "@mc_a"]),
    ("show_extension", ["show_extension", EXT]),
    ("show_extension_usages", ["show_extension", EXT, "--extension_usages=<root>"]),
    ("show_extension_innate", ["show_extension", "<root>//:MODULE.bazel%//:defs.bzl r"]),
    ("show_extension_missing", ["show_extension", "@mc_a//:ext.bzl%nope"]),
    # dump_repo_mapping
    ("dump_root", ["dump_repo_mapping", ""]),
    ("dump_two", ["dump_repo_mapping", "mc_a+", "mc_local+"]),
    ("dump_none", ["dump_repo_mapping"]),
    ("dump_missing", ["dump_repo_mapping", "nope+"]),
    # errors
    ("no_subcommand", []),
    ("bad_subcommand", ["frobnicate"]),
    ("graph_extra", ["graph", "x"]),
    ("graph_bad_output", ["graph", "--output=streamed_proto"]),
    ("show_extension_json", ["show_extension", "--output=json", EXT]),
    ("deps_missing", ["deps", "nope"]),
    ("deps_bad_version", ["deps", "mc_b@9.9"]),
    ("deps_bad_apparent", ["deps", "@nope"]),
    ("graph_all_repos", ["graph", "--all_repos"]),
    ("show_repo_two_modes", ["show_repo", "--all_repos", "@mc_a"]),
    ("tidy_extra", ["tidy", "x"]),
    ("base_module_multi", ["graph", "--base_module=mc_leaf", "--include_unused"]),
    ("bad_charset", ["graph", "--charset=latin1"]),
]

KEPT = re.compile(r"^(ERROR: |WARNING: |INFO: Repo )")


def normalize(text, output_base):
    text = text.replace(output_base, "${OUTPUT_BASE}")
    return text.replace(FIXTURE, "${FIXTURE}")


def main():
    shutil.rmtree(SCRATCH, ignore_errors=True)
    os.makedirs(HOME)
    os.makedirs(FIXTURE)
    for part in ("registry", "sources", "ws"):
        shutil.copytree(os.path.join(HERE, part), os.path.join(FIXTURE, part))
    env = {"PATH": "/usr/bin:/bin", "HOME": HOME, "TMPDIR": SCRATCH}
    prefix = [GENUINE, "--batch", "--ignore_all_rc_files", "--output_user_root=" + OUR,
              "--host_jvm_args=-Xmx2g", "mod"]
    common = ["--registry=file://" + FIXTURE + "/registry", "--registry=" + BCR,
              "--lockfile_mode=off", "--color=no", "--curses=no", "--noshow_progress"]
    output_base = None
    cases = []
    for name, args in CASES:
        started = time.time()
        result = subprocess.run(prefix + common + args, cwd=WS, env=env, capture_output=True)
        if output_base is None:
            info = subprocess.run([GENUINE, "--batch", "--ignore_all_rc_files",
                                   "--output_user_root=" + OUR, "info", "output_base"],
                                  cwd=WS, env=env, capture_output=True, text=True)
            output_base = info.stdout.strip()
        stderr = result.stderr.decode("utf-8", "replace")
        events = [normalize(line, output_base) for line in stderr.splitlines()
                  if KEPT.match(line)]
        stdout = result.stdout.decode("utf-8", "replace")
        cases.append({
            "name": name,
            "args": args,
            "exit_code": result.returncode,
            "stdout": normalize(stdout, output_base),
            "events": events,
            "elapsed_seconds": round(time.time() - started, 3),
        })
        print(name, result.returncode, flush=True)
    # `mod tidy` rewrites MODULE.bazel (buildozer of @bazel_tools): twice on
    # a private copy of tidy_ws/, the file recorded after each run.
    tidy_ws = SCRATCH + "/tidy_ws"
    shutil.copytree(os.path.join(HERE, "tidy_ws"), tidy_ws)
    tidy = []
    for run in range(2):
        result = subprocess.run(prefix + ["tidy", "--lockfile_mode=off", "--color=no", "--curses=no",
                                          "--noshow_progress"], cwd=tidy_ws, env=env,
                                capture_output=True)
        stderr = result.stderr.decode("utf-8", "replace")
        with open(tidy_ws + "/MODULE.bazel") as module:
            tidy.append({
                "exit_code": result.returncode,
                "stdout": result.stdout.decode("utf-8", "replace"),
                "events": [line.replace(tidy_ws, "${WORKSPACE}") for line in stderr.splitlines()
                           if re.match(r"^(ERROR|WARNING|INFO): ", line)
                           and not line.startswith("INFO: Repo ")],
                "module": module.read(),
            })
        print("tidy", run, result.returncode, flush=True)
    with open(GENUINE, "rb") as binary:
        digest = hashlib.sha256(binary.read()).hexdigest()
    receipt = {
        "bazel": GENUINE,
        "bazel_sha256": digest,
        "command_prefix": ["mod"] + [arg.replace(FIXTURE, "${FIXTURE}") for arg in common],
        "note": "events: ERROR/WARNING/`INFO: Repo` stderr lines; ${FIXTURE} is the copied fixture",
        "cases": cases,
        "tidy": tidy,
    }
    with open(os.path.join(HERE, "reference_receipt.json"), "w") as out:
        json.dump(receipt, out, indent=1, sort_keys=True)
        out.write("\n")


if __name__ == "__main__":
    main()
