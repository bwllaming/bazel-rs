#!/usr/bin/env python3
"""Genuine (or native) capture of the build's process tally around runfiles
trees: one executable `tool` with runfiles used as a tool by several actions
of several targets, in the exec and the target configuration, by a genrule,
beside a second tool and beside top-level builds of the tools themselves.
Each case builds a fresh copy of `workspace/` (next to this script) twice
(cold, then fully cached) and records the exit code and the
`INFO: N processes: ...` line of each build.

Bazel 9.3.1 runs a runfiles tree as four internal actions
(RunfilesSupport.create: SourceManifestAction for the .runfiles_manifest,
SymlinkTreeAction for .runfiles/MANIFEST, the RunfilesTreeAction of the tree
artifact, RepoMappingManifestAction for the .repo_mapping), once per
executable and configuration however many actions stage the tree.

  capture.py genuine <bazel 9.3.1> <scratch>
  capture.py native <bazel-rs> <scratch>
"""
import json, os, re, shutil, subprocess, sys

CASES = [
    ("one_exec_use", ["//t:e2"]),
    ("exec_uses_shared", ["//t:e1", "//t:e2", "//t:g"]),
    ("exec_and_target", ["//t:e1", "//t:t1"]),
    ("two_tools", ["//t:e1", "//t:f1"]),
    ("tool_top_level_and_used", ["//t:tool", "//t:e1", "//t:t1"]),
    ("all", ["//t:all"]),
    ("top_level_binary_only", ["//t:tool2"]),
]


def main():
    engine, binary, scratch = sys.argv[1], sys.argv[2], os.path.abspath(sys.argv[3])
    here = os.path.dirname(os.path.abspath(__file__))
    shutil.rmtree(scratch, ignore_errors=True)
    rows = {}
    for name, targets in CASES:
        ws = os.path.join(scratch, name, "ws")
        home = os.path.join(scratch, name, "home")
        shutil.copytree(os.path.join(here, "workspace"), ws)
        os.makedirs(home)
        env = {"PATH": "/usr/bin:/bin", "HOME": home}
        if engine == "genuine":
            full = [binary, "--nosystem_rc", "--nohome_rc", "--batch",
                    f"--output_user_root={scratch}/{name}/root", "build"] + targets
        else:
            full = [binary, "--nosystem_rc", "--nohome_rc",
                    f"--cache-dir={scratch}/{name}/cache", "build"] + targets
        runs = []
        for _ in range(2):
            run = subprocess.run(full, cwd=ws, env=env, capture_output=True, text=True)
            runs.append([run.returncode] + [line for line in run.stderr.splitlines()
                                            if re.match(r"^INFO: \d+ process", line)])
        rows[name] = {"targets": targets, "runs": runs}
    json.dump({"engine": engine, "rows": rows}, sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()
