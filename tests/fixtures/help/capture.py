"""Capture genuine Bazel 9.3.0 `help` topics for tests/help_command.rs and
scripts/extract_help_topics.py (src/help_topics.rs).

Scratch only (private HOME, --output_user_root and cwd under /tmp/opencode).
Run: python3 tests/fixtures/help/capture.py (writes reference_receipt.json here).
"""
import hashlib
import json
import os
import shutil
import subprocess

GENUINE = "/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64"
SCRATCH = "/tmp/opencode/help-capture"
HERE = os.path.dirname(os.path.abspath(__file__))
COMMANDS = ["aquery", "build", "canonicalize-flags", "clean", "config", "coverage", "cquery",
            "dump", "fetch", "help", "info", "license", "mobile-install", "mod", "print_action",
            "query", "run", "shutdown", "test", "vendor", "version"]
TOPICS = [[]] + [[command] for command in COMMANDS] + [
    ["startup_options"], ["target-syntax"], ["info-keys"], ["nonexistent"]]


def main():
    shutil.rmtree(SCRATCH, ignore_errors=True)
    os.makedirs(SCRATCH + "/home")
    env = {"PATH": "/usr/bin:/bin", "HOME": SCRATCH + "/home", "TMPDIR": SCRATCH}
    cases = []
    # Option usage lines are shared by many commands: each case lists
    # indices into `lines`.
    lines_table = []
    index = {}

    def intern(line):
        if line not in index:
            index[line] = len(lines_table)
            lines_table.append(line)
        return index[line]

    for topic in TOPICS:
        result = subprocess.run([GENUINE, "--batch", "--ignore_all_rc_files",
                                 "--output_user_root=" + SCRATCH + "/our", "help"] + topic,
                                cwd=SCRATCH + "/home", env=env, capture_output=True, text=True)
        case = {"args": topic, "exit_code": result.returncode,
                "errors": [line for line in result.stderr.splitlines() if line.startswith("ERROR: ")]}
        stdout = result.stdout
        if topic and topic[0] in COMMANDS + ["startup_options"]:
            # The help template, then per option category its heading and
            # option usage lines (OptionsUsage, medium verbosity).
            lines = stdout.split("\n")
            first = next((i for i, line in enumerate(lines) if line.startswith("  --")), len(lines))
            # The category heading precedes the first option line.
            case["template"] = "\n".join(lines[:max(first - 1, 0)])
            case["options"] = [intern(line) for line in lines[first - 1:] if line]
        else:
            case["stdout"] = stdout
        cases.append(case)
        print(topic, result.returncode, flush=True)
    with open(GENUINE, "rb") as binary:
        digest = hashlib.sha256(binary.read()).hexdigest()
    with open(os.path.join(HERE, "reference_receipt.json"), "w") as out:
        json.dump({"bazel": GENUINE, "bazel_sha256": digest, "lines": lines_table,
                   "cases": cases}, out, separators=(",", ":"))
        out.write("\n")
    shutil.rmtree(SCRATCH, ignore_errors=True)


if __name__ == "__main__":
    main()
