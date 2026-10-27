#!/usr/bin/env python3
"""Recompute BUILTINS_DIGEST (src/engine/query/formats.rs) from a genuine Bazel install.

Usage: python3 builtins_digest.py <install_base>/A-server.jar

StarlarkBuiltinsValue.transitiveDigest is the BzlLoadValue transitive digest of
@_builtins//:exports.bzl: SHA-256 over the file digest followed by the transitive
digests of its loads in statement order (BzlLoadFunction.java:858-879). Bundled
builtins live in ConfiguredRuleClassProvider.BundledFileSystem, whose file digest
is SHA-256 of the path "/virtual_builtins_bzl/<file>" rather than of the content.
"""
import hashlib, io, re, sys, zipfile

jar = zipfile.ZipFile(sys.argv[1])
bundle = zipfile.ZipFile(io.BytesIO(jar.read('com/google/devtools/build/lib/bazel/rules/builtins_bzl.zip')))
files = {n[len('builtins_bzl/'):]: bundle.read(n) for n in bundle.namelist() if not n.endswith('/')}
LOAD = re.compile(rb'^load\(\s*"([^"]+)"', re.M)
memo = {}


def digest(path):
    if path not in memo:
        h = hashlib.sha256(hashlib.sha256(('/virtual_builtins_bzl/' + path).encode()).digest())
        for match in LOAD.finditer(files[path]):
            label = match.group(1).decode()
            for prefix in ('@_builtins//:', '//:', ':'):
                if label.startswith(prefix):
                    h.update(digest(label[len(prefix):]))
                    break
            else:
                raise SystemExit(f'unexpected load {label} in {path}')
        memo[path] = h.digest()
    return memo[path]


print(digest('exports.bzl').hex())
