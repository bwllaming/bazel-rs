#!/usr/bin/env python3
"""Capture tests/fixtures/darwin_sandbox/reference_receipt.json from a macOS
host parity report (scripts/host_parity.py run --report on a Mac, e.g. the
macOS CI job's `host-parity-macos` artifact).

Usage: capture.py REPORT.json RUN_URL > reference_receipt.json

Takes genuine Bazel 9.3.1 darwin-arm64's side of the darwin_sandbox case:
per step the arguments, the exit code, the `INFO: N processes` strategies,
the probe outputs (what a spawn may read, write and reach) and, for the
`--sandbox_debug` step, the darwin-sandbox profiles. Host-specific paths
become placeholders: the user database home (`/Users/<user>`) `<USER_HOME>`,
the DARWIN_USER_TEMP_DIR / DARWIN_USER_CACHE_DIR directories
`<DARWIN_USER_TEMP_DIR>` / `<DARWIN_USER_CACHE_DIR>` (host_parity already
replaced the tools' own paths: `<SPAWN_ROOT>`, `<SPAWN_DIR>`, `<TMP>`).
tests/darwin_sandbox.rs replays it natively on macOS.
"""

import json
import re
import sys


def host_paths(text):
    text = re.sub(r"/private/var/folders/[^\"/]+/[^\"/]+/T\b", "<DARWIN_USER_TEMP_DIR>", text)
    text = re.sub(r"/private/var/folders/[^\"/]+/[^\"/]+/C\b", "<DARWIN_USER_CACHE_DIR>", text)
    return re.sub(r"/Users/[^\"/]+/Library/", "<USER_HOME>/Library/", text)


def main():
    report = json.load(open(sys.argv[1]))
    if report["host"] != "darwin-arm64":
        raise SystemExit(f"capture.py: a {report['host']} report")
    case = next(c for c in report["cases"] if c["name"] == "darwin_sandbox")
    steps = []
    for step in case["steps"]:
        items = {key: value["genuine"] for key, value in step["items"].items()}
        steps.append({
            "args": step["args"],
            "exit": int(items["exit"]),
            "processes": items["stderr"],
            "probes": {key[len("bazel-bin/"):-len(".txt")]: value
                       for key, value in sorted(items.items()) if key.startswith("bazel-bin/")},
            **({"profiles": host_paths(items["sandbox profiles"])} if "sandbox profiles" in items else {}),
        })
    json.dump({
        "bazel": "9.3.1 (bazel-9.3.1-darwin-arm64, official release)",
        "captured": "GitHub Actions macos-15 (arm64) runner: " + sys.argv[2],
        "invocation": "scripts/host_parity.py run (case darwin_sandbox): `bazel --batch "
                      "--ignore_all_rc_files --output_user_root=<private>` under `env -i PATH HOME TMPDIR LANG USER`",
        "steps": steps,
    }, sys.stdout, indent=1, sort_keys=True)
    print()


if __name__ == "__main__":
    main()
