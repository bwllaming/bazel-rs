#!/usr/bin/env python3
"""A JSON-protocol persistent worker: copies the file named in its input.

Arguments (one request): OUTPUT LISTING. LISTING names (on its first line)
a file that is not an input of the spawn: a sibling output of the action
that wrote LISTING (rules_kotlin's JdepsMerge reading the jar its jdeps
names).
"""
import json
import sys


def work(arguments):
    output, listing = arguments
    with open(listing) as handle:
        named = handle.readline().strip()
    with open(named) as handle:
        content = handle.read()
    with open(output, "w") as handle:
        handle.write(content)


def main():
    if "--persistent_worker" not in sys.argv:
        work(sys.argv[1:])
        return
    for line in sys.stdin:
        if not line.strip():
            continue
        request = json.loads(line)
        try:
            work(request.get("arguments", []))
            response = {"exitCode": 0, "output": ""}
        except Exception as error:  # reported to the build
            response = {"exitCode": 1, "output": "%s\n" % error}
        if "requestId" in request:
            response["requestId"] = request["requestId"]
        sys.stdout.write(json.dumps(response) + "\n")
        sys.stdout.flush()


main()
