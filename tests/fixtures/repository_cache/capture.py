"""Paired capture for tests/repository_cache.rs (docs/REPOSITORY_CACHE.md).

Genuine Bazel 9.3.1 and bazel-rs share one `--repository_cache` on this fixture:
an http_archive and an http_file of the root module and the registry archive of
module `dep` (a private HTTP registry), all served by a counting loopback server.

Steps (each engine in its own copy of workspace/, fresh output roots/caches):
  1. genuine, repository cache G (empty): downloads, writes G.
  2. genuine, fresh output root, G: what a second genuine output base fetches.
  3. bazel-rs, fresh cache, G (genuine-written): must download no archive.
  4. bazel-rs, fresh cache, repository cache N (empty): downloads, writes N.
  5. bazel-rs, another fresh cache, N: must download nothing.
  6. genuine, fresh output root, N (bazel-rs-written): must download no archive.
The receipt records each step's request paths and G's/N's layouts (fixture
entries only; canonical-id markers named by the id they hash).

Scratch only: --batch --ignore_all_rc_files, private output roots, HOME and
TMPDIR under /tmp/opencode/w; the offline builtin-module registry.
Run: python3 tests/fixtures/repository_cache/capture.py BAZEL_RS_BINARY
"""
import hashlib
import http.server
import json
import os
import shutil
import subprocess
import sys
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
GENUINE = "/home/coder/bazel/.direnv/reference/bazel-9.3.1-linux-x86_64"
BUILTIN = os.path.join(REPO, "tests/fixtures/builtin_modules/registry")
SCRATCH = "/tmp/opencode/w/repository-cache-capture"
QUERY = "@arch//:* + @blob//file:* + @dep//:*"
NATIVE = os.path.abspath(sys.argv[1])

LOG = []


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        LOG.append(self.path)
        path = os.path.join(SERVED, self.path.lstrip("/"))
        if ".." in self.path or not os.path.isfile(path):
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        data = open(path, "rb").read()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


shutil.rmtree(SCRATCH, ignore_errors=True)
os.makedirs(SCRATCH)
SERVED = os.path.join(SCRATCH, "served")
server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
PORT = server.server_address[1]
threading.Thread(target=server.serve_forever, daemon=True).start()


def templated(source, destination):
    for directory, _, files in os.walk(source):
        for name in files:
            src = os.path.join(directory, name)
            dst = os.path.join(destination, os.path.relpath(src, source))
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            data = open(src, "rb").read()
            if not name.endswith(".gz"):
                data = data.replace(b"@PORT@", str(PORT).encode())
            open(dst, "wb").write(data)


templated(os.path.join(HERE, "files"), os.path.join(SERVED, "files"))
templated(os.path.join(HERE, "registry"), os.path.join(SERVED, "registry"))
REGISTRY = f"http://127.0.0.1:{PORT}/registry"
for name in ("home", "tmp"):
    os.makedirs(os.path.join(SCRATCH, name))
ENV = {"PATH": "/usr/bin:/bin", "HOME": os.path.join(SCRATCH, "home"),
       "TMPDIR": os.path.join(SCRATCH, "tmp"), "LC_ALL": "C.UTF-8"}
COMMON = ["query", QUERY, "--registry=file://" + BUILTIN, "--registry=" + REGISTRY]


def workspace(name):
    path = os.path.join(SCRATCH, name)
    if not os.path.exists(path):
        templated(os.path.join(HERE, "workspace"), path)
    return path


def run(engine, ws, cache, store):
    LOG.clear()
    if engine == "genuine":
        argv = [GENUINE, "--batch", "--ignore_all_rc_files",
                "--output_user_root=" + os.path.join(SCRATCH, store),
                "--host_jvm_args=-Xmx768m"] + COMMON + [
                "--repository_cache=" + os.path.join(SCRATCH, cache),
                "--noshow_progress", "--color=no", "--curses=no"]
    else:
        argv = [NATIVE, "--ignore_all_rc_files", "--cache-dir=" + os.path.join(SCRATCH, store)
                ] + COMMON + ["--repository_cache=" + os.path.join(SCRATCH, cache)]
    p = subprocess.run(["nice", "-n", "10"] + argv, cwd=workspace(ws), env=ENV,
                       capture_output=True, text=True, timeout=900)
    if p.returncode != 0:
        raise SystemExit(f"{engine} exit {p.returncode}\n{p.stderr}")
    return {"stdout": sorted(p.stdout.split()), "requests": sorted(LOG)}


FIXTURE = {}
for name in ("files/arch.tar.gz", "files/blob.txt", "files/dep.tar.gz",
             "registry/modules/dep/1.0/MODULE.bazel", "registry/modules/dep/1.0/source.json",
             "registry/bazel_registry.json"):
    data = open(os.path.join(SERVED, name), "rb").read()
    FIXTURE[hashlib.sha256(data).hexdigest()] = name
    FIXTURE[hashlib.sha384(data).hexdigest()] = name
IDS = {
    f"http://127.0.0.1:{PORT}/files/{name}": f"http://127.0.0.1:<PORT>/files/{name}"
    for name in ("arch.tar.gz", "blob.txt", "dep.tar.gz")
}


def layout(cache):
    """The fixture entries of a repository cache, normalized."""
    root = os.path.join(SCRATCH, cache, "content_addressable")
    entries = []
    for algorithm in sorted(os.listdir(root)):
        for key in sorted(os.listdir(os.path.join(root, algorithm))):
            if key not in FIXTURE:
                continue
            names = []
            for name in sorted(os.listdir(os.path.join(root, algorithm, key))):
                if name.startswith("id-"):
                    hashed = {getattr(hashlib, algorithm)(cid.encode()).hexdigest(): template
                              for cid, template in IDS.items()}
                    names.append("id-<" + hashed.get(name[3:], "unknown " + name) + ">")
                else:
                    names.append(name)
            entries.append({"algorithm": algorithm, "file": FIXTURE[key], "names": names})
    return sorted(entries, key=lambda entry: (entry["file"], entry["algorithm"]))


steps = {
    "1_genuine_cold": run("genuine", "ws_genuine", "G", "our1"),
    "2_genuine_fresh_output_root_shared": run("genuine", "ws_genuine", "G", "our2"),
    "3_native_fresh_cache_on_genuine_written": run("native", "ws_native", "G", "nc1"),
}
genuine_layout = layout("G")
steps["4_native_cold"] = run("native", "ws_native2", "N", "nc2")
native_layout = layout("N")
steps["5_native_fresh_cache_shared"] = run("native", "ws_native2", "N", "nc3")
steps["6_genuine_fresh_output_root_on_native_written"] = run("genuine", "ws_genuine2", "N", "our3")
for step in steps.values():
    step["requests"] = [r.replace(str(PORT), "<PORT>") for r in step["requests"]]
receipt = {
    "about": __doc__.strip().splitlines()[0] + " Steps: see capture.py. requests: every GET "
             "the loopback server saw; layouts: <cache>/content_addressable entries of the "
             "fixture's blobs.",
    "binary": os.path.basename(GENUINE),
    "sha256": hashlib.sha256(open(GENUINE, "rb").read()).hexdigest(),
    "steps": steps,
    "genuine_layout": genuine_layout,
    "native_layout": native_layout,
}
server.shutdown()
shutil.rmtree(SCRATCH, ignore_errors=True)
with open(os.path.join(HERE, "reference_receipt.json"), "w") as out:
    json.dump(receipt, out, indent=1, sort_keys=True)
    out.write("\n")
print(json.dumps(receipt, indent=1, sort_keys=True))
