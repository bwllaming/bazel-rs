"""Capture genuine Bazel 9.3.0 `shutdown`, `clean` and `fetch` for tests/cmd_clean_fetch.rs.

Scratch only: a private workspace, HOME and --output_user_root under /tmp/opencode; a loopback
HTTP server (a thread of this script) serves remote.tar.gz (written next to this script, a
deterministic gzip tarball) for an http_archive. The cases run in order against ONE output base.
Run: python3 tests/fixtures/cmd_clean_fetch/capture.py (writes reference_receipt.json here).
"""
import gzip
import hashlib
import http.server
import io
import json
import os
import re
import shutil
import subprocess
import tarfile
import threading

GENUINE = "/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64"
SCRATCH = "/tmp/opencode/cmd-clean-fetch-capture"
WS = SCRATCH + "/ws"
OUR = SCRATCH + "/our"
HOME = SCRATCH + "/home"
HERE = os.path.dirname(os.path.abspath(__file__))
ARCHIVE = os.path.join(HERE, "remote.tar.gz")

CASES = [
    ("shutdown", ["shutdown"]),
    ("shutdown_iff", ["shutdown", "--iff_heap_size_greater_than=10"]),
    ("shutdown_residue", ["shutdown", "x"]),
    ("shutdown_unknown_option", ["shutdown", "--foo"]),
    ("sync", ["sync"]),
    ("fetch_targets", ["fetch", "//pkg:g"]),
    ("fetch_repo", ["fetch", "--repo=@remote"]),
    ("fetch_repo_canonical", ["fetch", "--repo=@@+http_archive+remote"]),
    ("fetch_repo_two", ["fetch", "--repo=@remote", "--repo=@bazel_tools"]),
    ("fetch_repo_not_visible", ["fetch", "--repo=@nope"]),
    ("fetch_repo_invalid", ["fetch", "--repo=remote"]),
    ("fetch_repo_undefined", ["fetch", "--repo=@@nope+"]),
    ("fetch_repo_failures", ["fetch", "--repo=@nope", "--repo=@@nope+"]),
    ("fetch_force", ["fetch", "--force", "--repo=@remote"]),
    ("fetch_conflict", ["fetch", "--all", "//pkg:g"]),
    ("fetch_nofetch", ["fetch", "--nofetch"]),
    ("fetch_configure", ["fetch", "--configure"]),
    ("fetch_missing_package", ["fetch", "//nope:x"]),
    ("build", ["build", "//pkg:g"]),
    ("clean", ["clean"]),
    ("clean_residue", ["clean", "x"]),
    ("clean_async", ["clean", "--async"]),
    ("build_after_clean", ["build", "//pkg:g"]),
    ("clean_expunge", ["clean", "--expunge"]),
    ("clean_expunge_async", ["clean", "--expunge_async"]),
    ("build_after_expunge", ["build", "//pkg:g"]),
    ("clean_expunge_and_async", ["clean", "--expunge", "--async"]),
]

# The stderr lines the receipt pins (progress, timing and Bazel's own build summary are not).
KEPT = re.compile(r"^(ERROR: |INFO: All |INFO: Starting clean|INFO: Output |Command ')")
OUTPUT = "execroot/_main/bazel-out/k8-fastbuild/bin/pkg/g.txt"


def archive():
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name, data in [
            ("remote-1/data.txt", b"remote data\n"),
            ("remote-1/BUILD.bazel", b'exports_files(["data.txt"])\n'),
        ]:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mtime = 0
            info.mode = 0o644
            tar.addfile(info, io.BytesIO(data))
    blob = gzip.compress(raw.getvalue(), mtime=0)
    with open(ARCHIVE, "wb") as f:
        f.write(blob)
    return blob


def normalize(text, output_base):
    text = re.sub(r"_tmp_\d+_[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "_tmp_<PID>_<UUID>", text)
    return text.replace(output_base, "<OUTPUT_BASE>").replace(WS, "<WORKSPACE>")


def remove_scratch():
    if os.path.exists(SCRATCH):
        subprocess.run(["chmod", "-R", "u+rwx", SCRATCH], check=False)
        shutil.rmtree(SCRATCH, ignore_errors=True)


def main():
    remove_scratch()
    os.makedirs(WS + "/pkg")
    os.makedirs(HOME)
    blob = archive()
    sha256 = hashlib.sha256(blob).hexdigest()
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            requests.append(self.path)
            if self.path == "/remote.tar.gz":
                self.send_response(200)
                self.send_header("Content-Length", str(len(blob)))
                self.end_headers()
                self.wfile.write(blob)
            else:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]
    module = (
        'module(name = "scratch")\n'
        'http_archive = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")\n'
        'http_archive(name = "remote", urls = ["http://127.0.0.1:{PORT}/remote.tar.gz"], '
        f'sha256 = "{sha256}", strip_prefix = "remote-1")\n'
    )
    build = 'genrule(name = "g", srcs = ["@remote//:data.txt"], outs = ["g.txt"], cmd = "cp $< $@")\n'
    with open(WS + "/MODULE.bazel", "w") as f:
        f.write(module.replace("{PORT}", str(port)))
    with open(WS + "/pkg/BUILD", "w") as f:
        f.write(build)
    output_base = OUR + "/" + hashlib.md5(WS.encode()).hexdigest()
    cases = []
    for name, args in CASES:
        before = len(requests)
        p = subprocess.run(
            ["env", "-i", "PATH=/usr/bin:/bin", "HOME=" + HOME, GENUINE, "--batch", "--ignore_all_rc_files",
             "--output_user_root=" + OUR] + args,
            cwd=WS, capture_output=True, text=True)
        stderr = [normalize(line, output_base) for line in p.stderr.splitlines()]
        cases.append({
            "name": name,
            "args": args,
            "exit": p.returncode,
            "stdout": normalize(p.stdout, output_base),
            "stderr": [line for line in stderr if KEPT.match(line)],
            "stderr_full": [line for line in stderr if "Xverify" not in line],
            "output_exists": os.path.exists(os.path.join(output_base, OUTPUT)),
            "downloads": len(requests) - before,
        })
    server.shutdown()
    receipt = {
        "about": "Genuine Bazel 9.3.0 shutdown/clean/fetch captures (tests/fixtures/cmd_clean_fetch/capture.py, "
                 "docs/COMMANDS.md): --batch --ignore_all_rc_files --output_user_root=<private>, env -i "
                 "PATH=/usr/bin:/bin HOME=<private>; the cases run in order against one output base. `stderr` "
                 "keeps the lines matching KEPT, `stderr_full` everything but the JVM warning; <OUTPUT_BASE> is "
                 "the output base (native: the workspace's state directory); `output_exists` = "
                 "<OUTPUT_BASE>/" + OUTPUT + " after the case; `downloads` = HTTP requests for the archive.",
        "genuine_sha256": hashlib.sha256(open(GENUINE, "rb").read()).hexdigest(),
        "kept_stderr": KEPT.pattern,
        "module": module,
        "build": build,
        "archive_sha256": sha256,
        "cases": cases,
    }
    with open(os.path.join(HERE, "reference_receipt.json"), "w") as f:
        json.dump(receipt, f, indent=1)
        f.write("\n")
    remove_scratch()


if __name__ == "__main__":
    main()
