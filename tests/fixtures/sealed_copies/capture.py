#!/usr/bin/env python3
"""Genuine (or native) capture of repository_ctx file operations on `cp -r`
copies of another repository's tree (in bazel-rs a sealed, read-only
published tree; in genuine its writable external/ directory): file,
template, patch, download over HTTP (a new directory and an existing file), delete
(a file and a whole copy), symlink and rename (a file, and a directory to
another parent). Prints the copies' listing and contents as JSON.

  capture.py --bazel <genuine bazel 9.3.1> --scratch <dir>     (genuine)
  capture.py --native <bazel-rs> --scratch <dir>               (native)
"""
import argparse, functools, http.server, json, os, shutil, subprocess, sys, threading

HERE = os.path.dirname(os.path.abspath(__file__))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bazel")
    parser.add_argument("--native")
    parser.add_argument("--scratch", required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    if os.path.exists(scratch):
        subprocess.run(["chmod", "-R", "u+w", scratch], check=True)
        shutil.rmtree(scratch)
    os.makedirs(scratch)
    ws = os.path.join(scratch, "ws")
    shutil.copytree(os.path.join(HERE, "workspace"), ws)
    home = os.path.join(scratch, "home")
    os.makedirs(home)
    env = {"PATH": "/usr/bin:/bin", "HOME": home}
    # The downloads' source (`payload.txt`, served over HTTP).
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=ws)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_address[1]}/payload.txt"
    if args.bazel:
        prefix = [args.bazel, "--nosystem_rc", "--nohome_rc", "--noworkspace_rc",
                  f"--output_user_root={scratch}/root", "--batch"]
        argv = prefix + ["build", f"--repository_cache={scratch}/repocache"]
    else:
        prefix = [args.native, "--ignore_all_rc_files", f"--cache-dir={scratch}/cache"]
        argv = prefix + ["build"]
    run = subprocess.run(argv + [f"--repo_env=PAYLOAD_URL={url}", "//:out"], cwd=ws, env=env, capture_output=True, text=True)
    if run.returncode != 0:
        sys.exit(f"exit {run.returncode}\n{run.stdout}{run.stderr}")
    bin_dir = subprocess.run(prefix + ["info", "bazel-bin"], cwd=ws, env=env,
                             capture_output=True, text=True, check=True).stdout.strip()
    out = open(os.path.join(bin_dir, "out.txt")).read()
    version = subprocess.run([args.bazel, "--version"], capture_output=True,
                             text=True).stdout.strip() if args.bazel else "native"
    json.dump({"reference": version, "out": out.splitlines()}, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
