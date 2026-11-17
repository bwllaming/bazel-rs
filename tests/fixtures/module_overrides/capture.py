#!/usr/bin/env python3
"""Capture genuine Bazel 9.3.1's observations of the module_overrides
fixtures (tests/module_overrides.rs) into reference_receipt.json.

usage: capture.py BAZEL_ELF SCRATCH_DIR

Genuine runs with --batch --ignore_all_rc_files in fresh copies under
SCRATCH_DIR (its built-in modules come from the default registry); the
fixtures themselves are local: a file:// archive and a local Git remote.
"""
import json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
BAZEL, SCRATCH = sys.argv[1], os.path.abspath(sys.argv[2])
os.makedirs(SCRATCH, exist_ok=True)
ENV = {"PATH": "/usr/bin:/bin", "HOME": os.path.join(SCRATCH, "home"),
       "TMPDIR": os.path.join(SCRATCH, "tmp"), "LC_ALL": "C"}
# Fresh output roots: a fetched repository would be reused (no DEBUG lines).
subprocess.run(["chmod", "-R", "u+w", os.path.join(SCRATCH, "out")], capture_output=True)
shutil.rmtree(os.path.join(SCRATCH, "out"), ignore_errors=True)
for d in ("home", "tmp"):
    os.makedirs(os.path.join(SCRATCH, d), exist_ok=True)

GIT_ENV = dict(ENV, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
               GIT_AUTHOR_NAME="fixture", GIT_AUTHOR_EMAIL="fixture@example.invalid",
               GIT_COMMITTER_NAME="fixture", GIT_COMMITTER_EMAIL="fixture@example.invalid",
               GIT_AUTHOR_DATE="2000-01-01T00:00:00Z", GIT_COMMITTER_DATE="2000-01-01T00:00:00Z")


def git(cwd, *args):
    return subprocess.run(["git", *args], cwd=cwd, env=GIT_ENV, check=True,
                          capture_output=True, text=True).stdout


def make_git_remote(path):
    """The git fixture's remote: `sub/` holds the module (and a submodule
    gitlink `sub/submod`), tagged v1 (an annotated tag)."""
    os.makedirs(os.path.join(path, "sub"))
    files = {
        "sub/MODULE.bazel": 'module(name = "gitmod", version = "3.0")\n',
        "sub/BUILD.bazel": 'filegroup(name = "all", srcs = glob(["*.txt"]), visibility = ["//visibility:public"])\n',
        "sub/data.txt": "from git\n",
        "top.txt": "outside the prefix\n",
    }
    for rel, text in files.items():
        with open(os.path.join(path, rel), "w") as f:
            f.write(text)
    git(path, "init", "--quiet", "-b", "main")
    git(path, "add", ".")
    git(path, "update-index", "--add", "--cacheinfo", "160000," + "1" * 40 + ",sub/submod")
    git(path, "commit", "--quiet", "-m", "fixture")
    git(path, "tag", "-a", "v1", "-m", "v1")


def run(ws, *args):
    out_root = os.path.join(SCRATCH, "out", os.path.basename(ws))
    p = subprocess.run([BAZEL, "--batch", "--ignore_all_rc_files", f"--output_user_root={out_root}", *args],
                       cwd=ws, env=ENV, capture_output=True, text=True)
    return p


def norm(text, ws):
    return text.replace(ws, "$WS")


def lines(p, ws, prefix):
    return [norm(l, ws) for l in p.stderr.splitlines() if l.startswith(prefix)]


def extensions_section(ws):
    text = open(os.path.join(ws, "MODULE.bazel.lock")).read()
    start = text.index('  "moduleExtensions": ')
    end = text.index('  "facts": ')
    return text[start:end]


def copy(name, dest):
    shutil.rmtree(dest, ignore_errors=True)
    shutil.copytree(os.path.join(HERE, name), dest)
    return dest


receipt = {"bazel": subprocess.run([BAZEL, "--version"], capture_output=True, text=True).stdout.strip()}

# repo_override
ws = copy("repo_override", os.path.join(SCRATCH, "repo_override"))
p = run(ws, "build", "//:g", "@gen_b//:x")
r = {"build_exit": p.returncode,
     "prints": sorted(set(re.sub(r"^DEBUG: [^ ]*: ", "", l) for l in lines(p, ws, "DEBUG: ") if "OVR|" in l)),
     "lockfile_extensions": extensions_section(ws)}
p = run(ws, "mod", "dump_repo_mapping", "", "+ext+gen_b")
r["root_mapping"], r["gen_b_mapping"] = [json.loads(l) for l in p.stdout.splitlines()]
VARIANTS = {
    "override_not_generated": 'override_repo(ext, nothere = "dep")\n',
    "inject_generated": 'inject_repo(ext, gen_d = "dep")\n',
    "invisible": 'override_repo(ext, gen_c = "depp")\n',
    "import_injected": 'inject_repo(ext, gen_b = "dep")\n',
    "duplicate": 'override_repo(ext, gen_c = "dep")\noverride_repo(ext, gen_c = "dep2")\n',
    "override_of_override": 'override_repo(ext, gen_b = "gen_a")\n',
    "bad_proxy": 'override_repo("x", gen_b = "dep")\n',
}
r["variants"] = {}
for name, extra in VARIANTS.items():
    vws = copy("repo_override", os.path.join(SCRATCH, "v-" + name))
    with open(os.path.join(vws, "MODULE.bazel"), "a") as f:
        f.write(extra)
    p = run(vws, "build", "--nobuild", "//:g")
    errors = [norm(l, vws) for l in p.stderr.splitlines() if l.startswith(("ERROR: ", "Error in "))]
    r["variants"][name] = {"exit": p.returncode, "errors": errors}
receipt["repo_override"] = r

# archive
for name, edit in [("archive", None), ("archive_no_integrity", "integrity")]:
    ws = copy("archive", os.path.join(SCRATCH, name))
    url = "file://" + os.path.join(ws, "arch.tar.gz")
    text = open(os.path.join(ws, "MODULE.bazel.in")).read().replace("@ARCHIVE_URL@", url)
    if edit:
        text = "".join(l for l in text.splitlines(True) if not l.strip().startswith(edit + " ="))
    open(os.path.join(ws, "MODULE.bazel"), "w").write(text)
    p = run(ws, "build", "//:g")
    receipt[name] = {"exit": p.returncode, "debug": lines(p, ws, "DEBUG: "),
                     "output": open(os.path.join(ws, "bazel-bin", "g.txt")).read() if p.returncode == 0 else None,
                     "lockfile_extensions": extensions_section(ws)}

# git
ws = copy("git", os.path.join(SCRATCH, "git"))
remote = os.path.join(SCRATCH, "git-remote")
shutil.rmtree(remote, ignore_errors=True)
make_git_remote(remote)
text = open(os.path.join(ws, "MODULE.bazel.in")).read().replace("@REMOTE@", "file://" + remote)
open(os.path.join(ws, "MODULE.bazel"), "w").write(text)
p = run(ws, "build", "//:g")
receipt["git"] = {"exit": p.returncode,
                  "debug": [l.replace(git(remote, "rev-parse", "v1^{commit}").strip(), "$COMMIT") for l in lines(p, ws, "DEBUG: ")],
                  "output": open(os.path.join(ws, "bazel-bin", "g.txt")).read() if p.returncode == 0 else p.stderr[-2000:]}

with open(os.path.join(HERE, "reference_receipt.json"), "w") as f:
    json.dump(receipt, f, indent=1, sort_keys=True)
    f.write("\n")
print(json.dumps(receipt, indent=1)[:6000])
