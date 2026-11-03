#!/usr/bin/env python3
"""Genuine (or native) capture of which sandbox backs the `sandboxed` strategy on this host
(SandboxModule.java:281-326, LinuxSandboxedSpawnRunner.isSupported) and what a spawn sees there:
the `INFO: N processes: ...` runner name, an explicit --spawn_strategy=linux-sandbox, and the
namespace-only options (--sandbox_fake_hostname, --sandbox_fake_username, the hermetic /tmp,
--sandbox_add_mount_pair) that processwrapper-sandbox ignores.

A one-genrule workspace is built per case in a scratch directory with a private HOME; genuine
runs `--batch` with a scratch --output_user_root, native a scratch --cache-dir. The host's
hostname and user name are replaced by <host> and <user>; a marker in the host /tmp tells
whether the spawn shares it. Output: JSON on stdout.

  capture.py --bazel <genuine bazel 9.3.1> --scratch <dir>
  capture.py --native <bazel-rs> --scratch <dir>
"""
import argparse, getpass, json, os, shutil, socket, subprocess, tempfile

MARKER_PREFIX = "sandbox-backend-marker-"
BUILD = ('genrule(name = "ok", outs = ["ok.txt"], cmd = "(echo host=$$(hostname); '
         'echo user=$$(id -un); if [ -e {marker} ]; then echo host_tmp=shared; '
         'else echo host_tmp=private; fi) > $@")\n')
CASES = [
    ("default", ["build", "//:ok"]),
    ("processwrapper", ["build", "--spawn_strategy=processwrapper-sandbox", "//:ok"]),
    ("linux-sandbox", ["build", "--spawn_strategy=linux-sandbox", "//:ok"]),
    ("fake-hostname-username",
     ["build", "--sandbox_fake_hostname", "--sandbox_fake_username", "//:ok"]),
    ("remount", ["build", "--sandbox_add_mount_pair=/usr:/opt/mp", "//:ok"]),
]


def lines_of(text, prefixes):
    out = []
    for line in text.splitlines():
        if any(line.startswith(prefix) for prefix in prefixes) and "memory limit" not in line:
            out.append(line)
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bazel")
    parser.add_argument("--native")
    parser.add_argument("--scratch", required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    shutil.rmtree(scratch, ignore_errors=True)
    host, user = socket.gethostname(), getpass.getuser()
    marker = tempfile.NamedTemporaryFile(prefix=MARKER_PREFIX, dir="/tmp")
    rows = []
    for name, argv in CASES:
        ws = os.path.join(scratch, name, "ws")
        home = os.path.join(scratch, name, "home")
        os.makedirs(ws)
        os.makedirs(home)
        for file, text in (("MODULE.bazel", 'module(name = "sb")\n'),
                           ("BUILD.bazel", BUILD.format(marker=marker.name))):
            with open(os.path.join(ws, file), "w") as handle:
                handle.write(text)
        env = {"PATH": "/usr/bin:/bin", "HOME": home}
        if args.bazel:
            full = [args.bazel, "--nosystem_rc", "--nohome_rc", "--batch",
                    f"--output_user_root={scratch}/{name}/root"] + argv[:1] + \
                   [f"--repository_cache={scratch}/repocache"] + argv[1:]
        else:
            full = [args.native, "--nosystem_rc", "--nohome_rc",
                    f"--cache-dir={scratch}/{name}/cache"] + argv
        run = subprocess.run(full, cwd=ws, env=env, capture_output=True, text=True)
        output = None
        if run.returncode == 0:
            if args.bazel:
                path = os.path.join(ws, "bazel-bin/ok.txt")
            else:
                path = next(os.path.join(root, "ok.txt") for root, _, files in
                            os.walk(os.path.join(scratch, name, "cache", "actions"))
                            if "ok.txt" in files)
            output = open(path).read().replace(f"host={host}\n", "host=<host>\n") \
                .replace(f"user={user}\n", "user=<user>\n")
        rows.append({"id": name, "args": argv, "exit": run.returncode,
                     "errors": lines_of(run.stderr, ("ERROR: ",)),
                     "processes": lines_of(run.stderr, ("INFO: ",))
                     and [line for line in lines_of(run.stderr, ("INFO: ",))
                          if " processes: " in line or " process: " in line],
                     "output": output})
    json.dump({"engine": "genuine" if args.bazel else "native", "rows": rows}, os.sys.stdout,
              indent=1)
    print()


if __name__ == "__main__":
    main()
