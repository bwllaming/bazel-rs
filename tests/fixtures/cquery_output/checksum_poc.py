#!/usr/bin/env python3
"""Proof of concept: genuine Bazel 9.3.1's configuration checksum
(BuildOptions.checksum) recomputed from a genuine `cquery --output=jsonproto`
configuration table. Usage: checksum_poc.py genuine/configured.jsonproto

BuildOptions.checksum = SHA-256 over protobuf writeStringNoTag (varint length +
UTF-8) of, in order: each FragmentOptions.cacheKey() sorted by class name
(`CLASS{name="value", ...}` over asMap(), options by name, EMPTY for empty
lists, NULL for null, `\\` and `"` escaped; `define` is listed under
user-defined in the dump and belongs back in CoreOptions), then
mapToCacheKey(starlark options), then mapToCacheKey(scopes) (each set Starlark
flag has scope DEFAULT). All five configurations of the capture reproduce.
"""
import hashlib, json, sys

def varint(n):
    out = b""
    while True:
        b, n = n & 0x7F, n >> 7
        if n:
            out += bytes([b | 0x80])
        else:
            return out + bytes([b])

def add(h, s):
    b = s.encode()
    h.update(varint(len(b)) + b)

def value(v):
    if v == "[]":
        return "EMPTY"
    if v == "null":
        return "NULL"
    return '"' + v.replace("\\", "\\\\").replace('"', '\\"') + '"'

def checksum(configuration):
    h = hashlib.sha256()
    fragments = configuration.get("fragmentOptions", [])
    user = [f for f in fragments if f["name"] == "user-defined"]
    user = user[0].get("options", []) if user else []
    defines = [o for o in user if o["name"].startswith("--define:")]
    starlark = [o for o in user if not o["name"].startswith("--define:")]
    for fragment in sorted(fragments, key=lambda f: f["name"]):
        if fragment["name"] == "user-defined":
            continue
        options = [(o["name"], o.get("value", "")) for o in fragment.get("options", [])]
        if fragment["name"].endswith(".CoreOptions"):
            joined = ", ".join(d["name"][len("--define:"):] + "=" + d["value"] for d in defines)
            options.append(("define", "[" + joined + "]"))
            options.sort()
        add(h, fragment["name"] + "{" + "".join(f"{n}={value(v)}, " for n, v in options) + "}")
    add(h, "".join(f'{o["name"]}={value(o.get("value", ""))}, ' for o in starlark))
    add(h, "".join(f'{o["name"]}="DEFAULT", ' for o in starlark))
    return h.hexdigest()

if __name__ == "__main__":
    result = json.load(open(sys.argv[1]))
    ok = True
    for configuration in result["configurations"]:
        computed = checksum(configuration)
        ok &= computed == configuration["checksum"]
        print(configuration["checksum"], computed == configuration["checksum"])
    sys.exit(0 if ok else 1)
