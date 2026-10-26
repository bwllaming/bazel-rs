#!/usr/bin/env python3
"""Paired capture for tests/test_args.rs: genuine Bazel 9.3.0 (--batch, private
output root) on a fresh copy of tests/fixtures/test_args/workspace; writes
tests/fixtures/test_args/reference_receipt.json."""
import hashlib, json, os, shutil, subprocess, sys, tempfile

REPO = sys.argv[1]
BAZEL = "/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64"
FIXTURE = os.path.join(REPO, "tests/fixtures/test_args")
ROOT = tempfile.mkdtemp(prefix="test-args-capture-", dir="/tmp/opencode")
WS = os.path.join(ROOT, "ws")
shutil.copytree(os.path.join(FIXTURE, "workspace"), WS)
STARTUP = ["--batch", "--ignore_all_rc_files", f"--output_user_root={ROOT}/root"]
FLAGS = [f"--repository_cache={ROOT}/repocache", "--lockfile_mode=off", "--noshow_progress",
         "--color=no", "--curses=no", "--define=MY_DEFINE=defined_value"]

def run(command, *args):
    p = subprocess.run([BAZEL, *STARTUP, command, *FLAGS, *args], cwd=WS, capture_output=True, text=True,
                       env={"PATH": "/usr/bin:/bin", "HOME": f"{ROOT}/home"})
    return p.returncode, p.stdout, p.stderr

def errors(stderr):
    return [l[len("ERROR: "):].replace(WS, "<ws>") for l in stderr.splitlines()
            if l.startswith("ERROR: ") and " in args attribute of " in l]

rows = {}
for target in ["//pkg:b", "//pkg:exec_tool"]:
    rc, out, err = run("run", target)
    rows[f"run {target}"] = {"exit": rc, "stdout": out,
                             "command_line": [l.split("Running command line: ", 1)[1].split(" ", 1)[1]
                                              for l in err.splitlines() if "Running command line: " in l]}
rc, out, err = run("test", "--test_output=all", "//pkg:t")
lines = out.splitlines() + err.splitlines()
start = lines.index("==================== Test output for //pkg:t:")
end = lines.index("=" * 80, start)
rows["test //pkg:t"] = {"exit": rc, "test_output": "\n".join(lines[start + 1:end]) + "\n"}
for target in ["//pkg:nodata_data"] + [f"//bad:{t}" for t in
               ["undefined", "undeclared", "multiple", "quote", "backslash", "braces", "dollar",
                "two_errors", "badlabel", "unterminated"]]:
    rc, out, err = run("build", "--nobuild", target)
    rows[f"build --nobuild {target}"] = {"exit": rc, "errors": errors(err)}

sha = lambda p: hashlib.sha256(open(p, "rb").read()).hexdigest()
receipt = {
    "what": "RunfilesSupport.computeArgs: `args` of executable/test rules through Expander.withDataLocations().tokenized (location functions with runfiles paths and data, Make variables, ShellUtils.tokenize)",
    "base_commit": subprocess.run(["git", "-C", REPO, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip(),
    "reference_sha256": sha(BAZEL),
    "capture_script_sha256": sha(__file__),
    "flags": {"reference": "--batch --ignore_all_rc_files --output_user_root=<private> --repository_cache=<private> --lockfile_mode=off --noshow_progress --color=no --curses=no --define=MY_DEFINE=defined_value"},
    "notes": ["Run 1 is the only run.", "errors keep Bazel's located `in args attribute` lines without the `ERROR: ` prefix; <ws> is the workspace."],
    "rows": rows,
}
with open(os.path.join(FIXTURE, "reference_receipt.json"), "w") as f:
    json.dump(receipt, f, indent=1, sort_keys=True)
    f.write("\n")
shutil.rmtree(ROOT, ignore_errors=True)
print(json.dumps(rows, indent=1))
