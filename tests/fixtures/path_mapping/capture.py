#!/usr/bin/env python3
"""Capture genuine Bazel 9.3.1's path mapping (`--experimental_output_paths=strip`)
on the fixture workspace into reference_receipt.json (docs/PATH_MAPPING.md).

Usage: capture.py GENUINE_BAZEL SCRATCH_DIR

The cases run in order in one output base (a fresh output user root under
SCRATCH_DIR); tests/path_mapping.rs replays them natively.
"""

import glob
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

# The exec-log fields compared for the mapped spawn.
LOG_FIELDS = ("commandArgs", "listedOutputs", "runner", "digest")

AQUERY = (
    'mnemonic("MyMnemonic|Coll|FileWrite", deps(//pkg:a) + //pkg:coll_same'
    " + //pkg:coll_diff + //pkg:u_noreq + //pkg:u_lit + //pkg:pw + //pkg:pw_tree"
    " + //pkg:pw_noreq)"
)

# (name, argv after the command word's flags, kind)
CASES = [
    # Before anything is cached: a mapped spawn needs a sandbox.
    (
        "local_strategy",
        ["build", "--experimental_output_paths=strip", "--spawn_strategy=local", "//pkg:coll_same"],
        "message",
    ),
    ("analysis", ["test", "//pkg:t", "//pkg:t_pw", "//pkg:t_mod"], "debug"),
    (
        "build_a",
        [
            "build",
            "--experimental_output_paths=strip",
            "--disk_cache=<DISK_CACHE>",
            "--execution_log_json_file=<LOG>",
            "//pkg:a",
        ],
        "outputs",
    ),
    (
        "build_b",
        [
            "build",
            "--experimental_output_paths=strip",
            "--disk_cache=<DISK_CACHE>",
            "--execution_log_json_file=<LOG>",
            "//pkg:b",
        ],
        "outputs",
    ),
    (
        "collisions",
        ["build", "--experimental_output_paths=strip", "//pkg:coll_same", "//pkg:coll_diff"],
        "outputs",
    ),
    (
        "param_writes",
        ["build", "--experimental_output_paths=strip", "//pkg:pw", "//pkg:pw_tree", "//pkg:pw_noreq"],
        "outputs",
    ),
    ("aquery", ["aquery", "--experimental_output_paths=strip", AQUERY], "aquery"),
]

# The outputs each build case reads (glob below the output tree).
OUTPUTS = {
    "build_a": ["*/bin/pkg/u.out", "*/bin/pkg/u.outdir"],
    "build_b": ["*/bin/pkg/u.out", "*/bin/pkg/u.outdir"],
    "collisions": ["*/bin/pkg/coll_same.out", "*/bin/pkg/coll_diff.out"],
    "param_writes": ["*/bin/pkg/pw.params", "*/bin/pkg/pw_tree.params", "*/bin/pkg/pw_noreq.params"],
}


def read_outputs(out, patterns):
    """Exec path -> content (a tree: its sorted `relative: content` lines)."""
    result = {}
    for pattern in patterns:
        for path in sorted(glob.glob(os.path.join(out, pattern))):
            exec_path = "bazel-out/" + os.path.relpath(path, out)
            if os.path.isdir(path):
                lines = []
                for root, _, files in sorted(os.walk(path)):
                    for name in sorted(files):
                        full = os.path.join(root, name)
                        with open(full) as handle:
                            lines.append(f"{os.path.relpath(full, path)}: {handle.read()}")
                result[exec_path] = "".join(lines)
            else:
                with open(path) as handle:
                    result[exec_path] = handle.read()
    return result


def mapped_spawns(log):
    """The MyMnemonic spawns of a JSON execution log, the compared fields."""
    decoder = json.JSONDecoder()
    text = open(log).read()
    index = 0
    spawns = []
    while index < len(text):
        while index < len(text) and text[index].isspace():
            index += 1
        if index >= len(text):
            break
        entry, index = decoder.raw_decode(text, index)
        if entry.get("mnemonic") != "MyMnemonic":
            continue
        record = {field: entry.get(field) for field in LOG_FIELDS}
        if record["digest"]:
            record["digest"] = record["digest"]["hash"]
        record["inputs"] = [[i["path"], i["digest"]["hash"]] for i in entry.get("inputs", [])]
        spawns.append(record)
    return spawns


def summary(stderr):
    """The `INFO: N processes: ...` line."""
    for line in stderr.splitlines():
        if re.match(r"INFO: \d+ process", line):
            return line
    return None


def main():
    genuine, scratch = sys.argv[1], os.path.abspath(sys.argv[2])
    if os.path.exists(scratch):
        subprocess.run(["chmod", "-R", "u+w", scratch], check=False)
        shutil.rmtree(scratch)
    ws = os.path.join(scratch, "ws")
    shutil.copytree(os.path.join(HERE, "workspace"), ws)
    home = os.path.join(scratch, "home")
    tmp = os.path.join(scratch, "tmp")
    os.makedirs(home)
    os.makedirs(tmp)
    env = {"PATH": "/usr/bin:/bin", "HOME": home, "TMPDIR": tmp, "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}
    startup = ["--batch", "--ignore_all_rc_files", f"--output_user_root={scratch}/out"]
    disk_cache = os.path.join(scratch, "disk_cache")
    cases = {}
    for name, argv, kind in CASES:
        log = os.path.join(scratch, f"{name}.log.json")
        concrete = [arg.replace("<DISK_CACHE>", disk_cache).replace("<LOG>", log) for arg in argv]
        flags = ["--noshow_progress", "--color=no", "--curses=no"]
        process = subprocess.run(
            [genuine, *startup, concrete[0], *flags, *concrete[1:]],
            cwd=ws,
            env=env,
            capture_output=True,
            text=True,
        )
        stderr = process.stderr.replace(ws, "<WS>")
        case = {"argv": argv, "kind": kind, "exit_code": process.returncode}
        if kind == "message":
            case["message"] = next(
                line.split(" failed: ", 1)[1] for line in stderr.splitlines() if " failed: " in line
            )
        elif kind == "debug":
            case["debug"] = [
                line.split(": ACT ", 1)[1] for line in stderr.splitlines() if ": ACT " in line
            ]
        elif kind == "outputs":
            out = os.path.realpath(os.path.join(ws, "bazel-out"))
            case["outputs"] = read_outputs(out, OUTPUTS[name])
            case["summary"] = summary(stderr)
            if os.path.exists(log):
                case["spawns"] = mapped_spawns(log)
        elif kind == "aquery":
            case["aquery"] = [
                line for line in process.stdout.splitlines() if not line.startswith("# Configuration:")
            ]
        cases[name] = case
    receipt = {
        "schema": "path-mapping-reference-receipt/v1",
        "fixture": "path_mapping",
        "purpose": "--experimental_output_paths=strip (PathMappers, StrippingPathMapper): mapped command lines in Target.actions and aquery (File arguments, map_each over File.path/dirname/root, the executable, parameter files, tree artifacts), the collision fallback (same and different contents), mapped execution (inputs staged and outputs produced at bazel-out/cfg paths, published at their own), the action keys (FOR_FINGERPRINTING, the mapper GUID), one disk cache entry for two configurations, ParameterFileWriteAction contents, --modify_execution_info set by a transition, the sandboxing requirement",
        "reference": {
            "binary": os.path.basename(genuine),
            "sha256": hashlib.sha256(open(genuine, "rb").read()).hexdigest(),
            "startup": "--batch --ignore_all_rc_files --output_user_root=<private scratch>",
            "command_flags": "<command> --noshow_progress --color=no --curses=no <argv>",
            "environment": "env -i PATH=/usr/bin:/bin HOME/TMPDIR private LANG=C.UTF-8 LC_ALL=C.UTF-8; one workspace and output base, the cases in order",
        },
        "cases": cases,
    }
    with open(os.path.join(HERE, "reference_receipt.json"), "w") as handle:
        json.dump(receipt, handle, indent=1, sort_keys=False)
        handle.write("\n")


if __name__ == "__main__":
    main()
