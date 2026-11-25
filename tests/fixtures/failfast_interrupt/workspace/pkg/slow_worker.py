#!/usr/bin/env python3
"""A JSON persistent worker whose every request takes 15 seconds."""
import json
import sys
import time


def work(arguments):
    time.sleep(15.0733)
    with open(arguments[0], "w") as handle:
        handle.write("done\n")


def main():
    if "--persistent_worker" not in sys.argv:
        with open(sys.argv[1][1:]) as handle:
            work(handle.read().splitlines())
        return 0
    for line in sys.stdin:
        if not line.strip():
            continue
        request = json.loads(line)
        arguments = []
        for argument in request.get("arguments", []):
            if argument.startswith("@"):
                with open(argument[1:]) as handle:
                    arguments += handle.read().splitlines()
            else:
                arguments.append(argument)
        work(arguments)
        sys.stdout.write(json.dumps({"exitCode": 0, "output": ""}) + "\n")
        sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
