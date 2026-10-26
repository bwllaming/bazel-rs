#!/usr/bin/env python3
"""A persistent worker speaking Bazel's JSON and proto worker protocols.

Without --persistent_worker it runs once from its @flagfile, like any tool.
With $TRACE set (the `trace` --define) it appends one JSON line per process
start and per WorkRequest to $TRACE/trace.jsonl.
"""
import base64
import fcntl
import json
import os
import sys


def trace(record):
    directory = os.environ.get("TRACE", "")
    if not directory:
        return
    record["pid"] = os.getpid()
    with open(os.path.join(directory, "trace.jsonl"), "a") as handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        handle.write(json.dumps(record, sort_keys=True) + "\n")


def work(arguments):
    """Runs one unit of work; returns (exit code, output)."""
    options = {}
    rest = list(arguments)
    while rest:
        name = rest.pop(0)
        if name in ("--in", "--out", "--mode") and rest:
            options[name[2:]] = rest.pop(0)
    mode = options.get("mode", "upper")
    if mode == "fail":
        return 1, "boom: work failed\n"
    if mode == "crash":
        sys.stderr.write("worker crashing\n")
        sys.stderr.flush()
        os._exit(3)
    if mode == "garbage":
        sys.stdout.write("this is not a response\n")
        sys.stdout.flush()
        os._exit(0)
    with open(options["in"]) as handle:
        data = handle.read()
    with open(options["out"], "w") as handle:
        handle.write(data.upper())
    if mode == "noisy":
        return 0, "noisy worker says hello\n"
    return 0, ""


def varint(stream):
    shift = result = 0
    while True:
        byte = stream.read(1)
        if not byte:
            return None
        result |= (byte[0] & 0x7F) << shift
        if not byte[0] & 0x80:
            return result
        shift += 7


def encode_varint(value):
    out = bytearray()
    if value < 0:
        value += 1 << 64
    while True:
        bits = value & 0x7F
        value >>= 7
        if value:
            out.append(bits | 0x80)
        else:
            out.append(bits)
            return bytes(out)


def fields(data):
    position = 0
    while position < len(data):
        key = 0
        shift = 0
        while True:
            byte = data[position]
            position += 1
            key |= (byte & 0x7F) << shift
            shift += 7
            if not byte & 0x80:
                break
        number, wire = key >> 3, key & 7
        if wire == 0:
            value = 0
            shift = 0
            while True:
                byte = data[position]
                position += 1
                value |= (byte & 0x7F) << shift
                shift += 7
                if not byte & 0x80:
                    break
            yield number, value
        elif wire == 2:
            length = 0
            shift = 0
            while True:
                byte = data[position]
                position += 1
                length |= (byte & 0x7F) << shift
                shift += 7
                if not byte & 0x80:
                    break
            yield number, data[position : position + length]
            position += length
        else:
            raise ValueError("unexpected wire type %d" % wire)


def read_proto(stream):
    length = varint(stream)
    if length is None:
        return None
    data = stream.read(length)
    request = {"arguments": [], "inputs": [], "requestId": 0, "wire": data.hex()}
    for number, value in fields(data):
        if number == 1:
            request["arguments"].append(value.decode())
        elif number == 2:
            entry = {}
            for inner, inner_value in fields(value):
                if inner == 1:
                    entry["path"] = inner_value.decode()
                elif inner == 2:
                    entry["digest"] = inner_value.decode()
            request["inputs"].append(entry)
        elif number == 3:
            request["requestId"] = value
        elif number == 5:
            request["verbosity"] = value
        elif number == 6:
            request["sandboxDir"] = value.decode()
    return request


def write_proto(stream, exit_code, output, request_id):
    body = bytearray()
    if exit_code:
        body += b"\x08" + encode_varint(exit_code)
    if output:
        raw = output.encode()
        body += b"\x12" + encode_varint(len(raw)) + raw
    if request_id:
        body += b"\x18" + encode_varint(request_id)
    stream.write(encode_varint(len(body)) + bytes(body))
    stream.flush()


def read_json(stream):
    line = stream.readline()
    if not line:
        return None
    request = json.loads(line)
    request["line"] = line.decode().rstrip("\n")
    for entry in request.get("inputs", []):
        if "digest" in entry:
            entry["digest"] = base64.b64decode(entry["digest"]).decode()
    request.setdefault("arguments", [])
    request.setdefault("inputs", [])
    request.setdefault("requestId", 0)
    return request


def main():
    argv = sys.argv
    if "--persistent_worker" not in argv:
        arguments = []
        for argument in argv[1:]:
            if argument.startswith("@"):
                with open(argument[1:]) as handle:
                    arguments += handle.read().splitlines()
            else:
                arguments.append(argument)
        code, output = work(arguments)
        sys.stderr.write(output)
        return code
    proto = os.environ.get("WORKER_PROTOCOL") != "json"
    trace(
        {
            "event": "start",
            "argv0_absolute": os.path.isabs(argv[0]),
            "argv0_tail": "/".join(argv[0].split("/")[-2:]),
            "args": argv[1:],
            "env": sorted(os.environ),
            "tmpdir": os.environ.get("TMPDIR"),
        }
    )
    stdin, stdout = sys.stdin.buffer, sys.stdout.buffer
    handled = 0
    while True:
        request = None
        # The protocol is sniffed from the first byte: '{' starts JSON.
        first = stdin.peek(1)[:1] if hasattr(stdin, "peek") else b""
        if not first:
            return 0
        proto = first != b"{"
        request = read_proto(stdin) if proto else read_json(stdin)
        if request is None:
            return 0
        handled += 1
        visible = {
            entry["path"]: os.path.exists(entry["path"]) for entry in request["inputs"]
        }
        trace(
            {
                "event": "request",
                "protocol": "proto" if proto else "json",
                "handled": handled,
                "request": request,
                "inputs_visible": visible,
            }
        )
        code, output = work(request["arguments"])
        if proto:
            write_proto(stdout, code, output, request["requestId"])
        else:
            response = {"exitCode": code, "output": output}
            if request["requestId"]:
                response["requestId"] = request["requestId"]
            stdout.write((json.dumps(response) + "\n").encode())
            stdout.flush()


if __name__ == "__main__":
    sys.exit(main())
