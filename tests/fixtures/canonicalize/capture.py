"""Capture genuine Bazel 9.3.0 `canonicalize-flags` for tests/canonicalize_flags.rs.

Scratch only: a private workspace (empty MODULE.bazel), HOME and --output_user_root
under /tmp/opencode. Run: python3 tests/fixtures/canonicalize/capture.py.
"""
import hashlib
import json
import os
import shutil
import subprocess

GENUINE = "/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64"
SCRATCH = "/tmp/opencode/canonicalize-capture"
HERE = os.path.dirname(os.path.abspath(__file__))
CASES = [
    ("none", []),
    ("basic", ["--", "--jobs", "3", "-c", "opt", "--keep_going", "--nokeep_going",
               "--copt=-O2", "--copt", "-g", "--define=a=b"]),
    ("last_wins_order", ["--", "--compilation_mode=dbg", "--jobs=2", "-c", "opt"]),
    ("booleans", ["--", "--stamp=yes", "--nobuild_runfile_links", "--keep_going=false",
                  "--experimental_google_legacy_api"]),
    ("tristate", ["--", "--enable_runfiles", "--noenable_runfiles", "--enable_runfiles=auto"]),
    ("expansion", ["--", "--experimental_spawn_scheduler", "--jobs=4"]),
    ("old_name", ["--", "--experimental_action_cache_store_output_metadata"]),
    ("for_query", ["--for_command=query", "--", "--output=graph", "--keep_going",
                   "--noimplicit_deps"]),
    ("unknown_option", ["--", "--bogus_option"]),
    ("residue", ["--", "//some:target"]),
    ("bad_command", ["--for_command=nope", "--", "--jobs=2"]),
    ("not_for_command", ["--for_command=query", "--", "--copt=x"]),
]


def main():
    shutil.rmtree(SCRATCH, ignore_errors=True)
    os.makedirs(SCRATCH + "/home")
    os.makedirs(SCRATCH + "/ws")
    open(SCRATCH + "/ws/MODULE.bazel", "w").close()
    env = {"PATH": "/usr/bin:/bin", "HOME": SCRATCH + "/home", "TMPDIR": SCRATCH}
    cases = []
    for name, args in CASES:
        result = subprocess.run([GENUINE, "--batch", "--ignore_all_rc_files",
                                 "--output_user_root=" + SCRATCH + "/our", "canonicalize-flags",
                                 "--lockfile_mode=off"] + args,
                                cwd=SCRATCH + "/ws", env=env, capture_output=True, text=True)
        cases.append({"name": name, "args": args, "exit_code": result.returncode,
                      "stdout": result.stdout,
                      "errors": [line for line in result.stderr.splitlines()
                                 if line.startswith("ERROR: ")]})
        print(name, result.returncode, flush=True)
    with open(GENUINE, "rb") as binary:
        digest = hashlib.sha256(binary.read()).hexdigest()
    with open(os.path.join(HERE, "reference_receipt.json"), "w") as out:
        json.dump({"bazel": GENUINE, "bazel_sha256": digest, "cases": cases}, out, indent=1)
        out.write("\n")
    shutil.rmtree(SCRATCH, ignore_errors=True)


if __name__ == "__main__":
    main()
