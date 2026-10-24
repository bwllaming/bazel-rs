//! N23f CLI coverage: runfiles trees, MANIFEST and `_repo_mapping` files,
//! tool runfiles in actions and the `run` command (docs/RUNFILES_TREES.md).
//! The fixture constants are shared verbatim with the paired capture
//! (artifacts/runfiles_trees-capture.py extracts them from this file).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");
include!("support/verify_env.rs");

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const MODULE: &str = r#"module(name = "rf")
bazel_dep(name = "ext")
local_path_override(module_name = "ext", path = "ext")
"#;

const EXT_MODULE: &str = r#"module(name = "ext")
"#;

const EXT_BUILD: &str = r#"exports_files(["e.txt"])
filegroup(name = "fg", srcs = ["e.txt"], visibility = ["//visibility:public"])
"#;

const DEFS: &str = r##"def _bin(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, ctx.attr.script, is_executable = True)
    gen = ctx.actions.declare_file(ctx.label.name + ".gen")
    ctx.actions.write(gen, "generated\n")
    data = ctx.files.data
    rf = ctx.runfiles(
        files = data + [gen],
        symlinks = {"sym/" + data[0].basename: data[0]} if ctx.attr.links else {},
        root_symlinks = {"rootsym.txt": data[0], "ext+/rooted.txt": gen} if ctx.attr.links else {},
    )
    for d in ctx.attr.deps:
        rf = rf.merge(d[DefaultInfo].default_runfiles)
    return [
        DefaultInfo(executable = exe, runfiles = rf),
        RunEnvironmentInfo(environment = {"FROM_TARGET": "yes"}, inherited_environment = ["INHERIT_ME"]),
    ]

my_bin = rule(_bin, executable = True, attrs = {
    "data": attr.label_list(allow_files = True),
    "deps": attr.label_list(),
    "script": attr.string(default = "#!/bin/sh\necho hi\n"),
    "links": attr.bool(default = True),
})

def _test(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\necho test\n", is_executable = True)
    return [DefaultInfo(executable = exe)]

my_test = rule(_test, test = True)

def _use(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    tool = ctx.attr.tool[DefaultInfo].files_to_run
    ctx.actions.run_shell(
        outputs = [out],
        tools = [tool] if ctx.attr.provider else [ctx.executable.tool],
        command = "(cd $1.runfiles && for f in $(find . | LC_ALL=C sort); do if [ -d $f ]; then echo \"$f/\"; else echo \"$f: $(cat $f | tr '\\n' '|')\"; fi; done) > $2; ls $(dirname $1) | LC_ALL=C sort >> $2; $1 >> $2",
        arguments = [ctx.executable.tool.path, out.path],
    )
    return [DefaultInfo(files = depset([out]))]

use_tool = rule(_use, attrs = {
    "tool": attr.label(executable = True, cfg = "target"),
    "provider": attr.bool(),
})

def _manifests(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    f = ctx.attr.tool[DefaultInfo].files_to_run
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [f.runfiles_manifest, f.repo_mapping_manifest],
        command = "cut -d' ' -f1 $1 > $3; echo === >> $3; cat $2 >> $3",
        arguments = [f.runfiles_manifest.path, f.repo_mapping_manifest.path, out.path],
    )
    return [DefaultInfo(files = depset([out]))]

manifests = rule(_manifests, attrs = {"tool": attr.label(executable = True, cfg = "target")})

def _conflict(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, "#!/bin/sh\n", is_executable = True)
    d = ctx.files.data[0]
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(symlinks = {"dir": d, "dir/x": d}))]

conflict = rule(_conflict, executable = True, attrs = {"data": attr.label_list(allow_files = True)})
"##;

const BUILD: &str = r##"load(":defs.bzl", "conflict", "manifests", "my_bin", "my_test", "use_tool")
my_bin(
    name = "b",
    data = ["d.txt", "@ext//data:e.txt"],
    args = ["fixed"],
    script = "#!/bin/bash\necho \"argv0=${0#$BUILD_EXECROOT/}\"\necho \"args=$*\"\necho \"cwd=${PWD#$BUILD_EXECROOT/}\"\nenv | LC_ALL=C sort | grep -E '^(BUILD_WORKSPACE_DIRECTORY|BUILD_WORKING_DIRECTORY|RUNFILES|JAVA_RUNFILES|TEST_SRCDIR|FROM_TARGET|INHERIT_ME|CLIENT_|RUN_SET)'\nls\ncat pkg/d.txt ../ext+/data/e.txt\nls \"$0.runfiles\"\nread line\necho \"stdin=$line\"\nexit 3\n",
)
my_bin(name = "tool", data = ["d.txt", "@ext//data:fg"], links = False, script = "#!/bin/sh\necho tool-ran\n")
my_bin(name = "outer", data = ["d.txt"], deps = [":tool"])
use_tool(name = "use", tool = ":tool")
use_tool(name = "use_provider", tool = ":tool", provider = True)
manifests(name = "m", tool = ":tool")
conflict(name = "conflict", data = ["d.txt"])
my_test(name = "t_test")
filegroup(name = "group", srcs = ["d.txt"])
exports_files(["script.sh"])
"##;

const PY_BUILD: &str = r#"load(":defs.bzl", "pybin")
pybin(name = "p", srcs = ["//py:srcs"])
"#;

const PY_DEFS: &str = r##"def _p(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, "#!/bin/sh\n", is_executable = True)
    rf = ctx.runfiles(files = ctx.files.srcs)
    rf = py_internal.merge_runfiles_with_generated_inits_empty_files_supplier(ctx = ctx, runfiles = rf)
    print("EMPTY " + json.encode(rf.empty_filenames.to_list()))
    return [DefaultInfo(executable = exe, runfiles = rf)]

pybin = rule(_p, executable = True, attrs = {"srcs": attr.label_list(allow_files = True)})
"##;

const PY_SRCS_BUILD: &str = r#"filegroup(name = "srcs", srcs = ["a/b/m.py", "a/x.so", "top.py", "__pycache__/c.pyc"], visibility = ["//visibility:public"])
"#;

/// The fixture: relative path, contents, mode.
fn fixture() -> Vec<(&'static str, &'static str, u32)> {
    vec![
        ("MODULE.bazel", MODULE, 0o644),
        ("ext/MODULE.bazel", EXT_MODULE, 0o644),
        ("ext/data/BUILD.bazel", EXT_BUILD, 0o644),
        ("ext/data/e.txt", "ext-data\n", 0o644),
        ("pkg/defs.bzl", DEFS, 0o644),
        ("pkg/BUILD.bazel", BUILD, 0o644),
        ("pkg/d.txt", "main-data\n", 0o644),
        (
            "pkg/script.sh",
            "#!/bin/sh\necho \"source-script $*\"; pwd\n",
            0o755,
        ),
        ("tools/build_defs/python/BUILD.bazel", PY_BUILD, 0o644),
        ("tools/build_defs/python/defs.bzl", PY_DEFS, 0o644),
        ("py/BUILD.bazel", PY_SRCS_BUILD, 0o644),
        ("py/a/b/m.py", "", 0o644),
        ("py/a/x.so", "", 0o644),
        ("py/top.py", "", 0o644),
        ("py/__pycache__/c.pyc", "", 0o644),
    ]
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("runfiles-trees-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        for (name, text, mode) in fixture() {
            let path = ws.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_mode(&path, text, mode);
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: &str, mode: u32) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_mode(&path, text, mode);
    }

    fn command(&self, args: &[&str], cwd: &str) -> Command {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path().join(cwd))
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path())
            .env("CLIENT_X", "1")
            .env("CLIENT_UNSET", "gone")
            .env("INHERIT_ME", "inherited")
            .env("RUNFILES_DIR", "/should/be/cleared");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(args, "")
    }

    fn run_in(&self, args: &[&str], cwd: &str) -> Output {
        let output = self.command(args, cwd).output().unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    fn cache(&self) -> PathBuf {
        self.path().join("private-cache")
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", stderr(output));
}

/// `label\tpath` lines of a successful build.
fn outputs(output: &Output) -> BTreeMap<String, PathBuf> {
    success(output);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (label, path) = line.split_once('\t').unwrap();
            (label.to_owned(), PathBuf::from(path))
        })
        .collect()
}

/// The published file `exec` (an exec path below `bazel-out/<cfg>/bin`) in
/// the cache, found by its exec path.
fn published(ws: &Workspace, exec: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(ws.cache().join("actions")).unwrap() {
        let path = entry.unwrap().path().join("outputs").join(exec);
        if fs::symlink_metadata(&path).is_ok() {
            found.push(path);
        }
    }
    found
}

fn single(ws: &Workspace, exec: &str) -> PathBuf {
    let found = published(ws, exec);
    assert_eq!(found.len(), 1, "{exec}: {found:?}");
    found.into_iter().next().unwrap()
}

/// Normalize native absolute paths: the workspace, published outputs.
fn normalize(ws: &Workspace, text: &str) -> String {
    let actions = format!("{}/actions/", ws.cache().display());
    let mut out = String::new();
    for line in text.lines() {
        let mut line = line.to_owned();
        while let Some(start) = line.find(&actions) {
            let rest = &line[start + actions.len()..];
            let end = rest
                .find("/outputs/")
                .map(|i| i + "/outputs/".len())
                .unwrap_or(0);
            line = format!("{}<OUT>/{}", &line[..start], &rest[end..]);
        }
        out.push_str(&line.replace(&ws.path().display().to_string(), "<WS>"));
        out.push('\n');
    }
    out
}

/// Every entry below a runfiles tree: `path -> link target`, `path/`, or
/// `path (empty)`.
fn listing(ws: &Workspace, root: &Path) -> Vec<String> {
    let mut entries = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in fs::read_dir(root.join(&relative)).unwrap() {
            let name = relative.join(entry.unwrap().file_name());
            let path = root.join(&name);
            let metadata = fs::symlink_metadata(&path).unwrap();
            let shown = name.display().to_string();
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap();
                entries.push(
                    normalize(ws, &format!("{shown} -> {}", target.display()))
                        .trim()
                        .to_owned(),
                );
            } else if metadata.is_dir() {
                entries.push(format!("{shown}/"));
                pending.push(name);
            } else {
                assert_eq!(metadata.len(), 0, "{shown}");
                assert_eq!(metadata.permissions().mode() & 0o777, 0o644, "{shown}");
                entries.push(format!("{shown} (empty)"));
            }
        }
    }
    entries.sort();
    entries
}

const B_TREE: &str = "\
MANIFEST -> <OUT>/bazel-out/k8-fastbuild/bin/pkg/b.runfiles_manifest
_main/
_main/pkg/
_main/pkg/b -> <OUT>/bazel-out/k8-fastbuild/bin/pkg/b
_main/pkg/b.gen -> <OUT>/bazel-out/k8-fastbuild/bin/pkg/b.gen
_main/pkg/d.txt -> <WS>/pkg/d.txt
_main/sym/
_main/sym/d.txt -> <WS>/pkg/d.txt
_repo_mapping -> <OUT>/bazel-out/k8-fastbuild/bin/pkg/b.repo_mapping
ext+/
ext+/data/
ext+/data/e.txt -> <WS>/ext/data/e.txt
ext+/rooted.txt -> <OUT>/bazel-out/k8-fastbuild/bin/pkg/b.gen
rootsym.txt -> <WS>/pkg/d.txt";

const B_MANIFEST: &str = "\
_main/pkg/b <OUT>/bazel-out/k8-fastbuild/bin/pkg/b
_main/pkg/b.gen <OUT>/bazel-out/k8-fastbuild/bin/pkg/b.gen
_main/pkg/d.txt <WS>/pkg/d.txt
_main/sym/d.txt <WS>/pkg/d.txt
_repo_mapping <OUT>/bazel-out/k8-fastbuild/bin/pkg/b.repo_mapping
ext+/data/e.txt <WS>/ext/data/e.txt
ext+/rooted.txt <OUT>/bazel-out/k8-fastbuild/bin/pkg/b.gen
rootsym.txt <WS>/pkg/d.txt
";

#[test]
fn executable_targets_build_runfiles_trees_manifests_and_repo_mappings() {
    let ws = Workspace::new();
    let built = outputs(&ws.run(&["build", "//pkg:b"]));
    // Only the files to build are reported.
    assert_eq!(built.keys().collect::<Vec<_>>(), ["//pkg:b"]);
    let manifest = single(&ws, "bazel-out/k8-fastbuild/bin/pkg/b.runfiles/MANIFEST");
    let tree = manifest.parent().unwrap();
    assert_eq!(listing(&ws, tree).join("\n"), B_TREE);
    // The MANIFEST is a symlink to the input manifest (SymlinkTreeHelper.linkManifest).
    let input = fs::read_link(&manifest).unwrap();
    assert_eq!(
        normalize(&ws, &fs::read_to_string(&input).unwrap()),
        B_MANIFEST
    );
    assert_eq!(
        fs::metadata(&input).unwrap().permissions().mode() & 0o777,
        0o555
    );
    // Main repository entries, then the external repository's (its own
    // mapping), then the module name; root symlinks under ext+ count.
    let mapping = single(&ws, "bazel-out/k8-fastbuild/bin/pkg/b.repo_mapping");
    assert_eq!(
        fs::read_to_string(mapping).unwrap(),
        ",ext,ext+\n,rf,_main\next+,ext,ext+\n"
    );
    // The executable's sibling (a hardlink of its store blob) resolves
    // `$0.runfiles`.
    use std::os::unix::fs::MetadataExt;
    let sibling = fs::symlink_metadata(tree.parent().unwrap().join("b")).unwrap();
    assert!(sibling.is_file());
    assert_eq!(
        sibling.ino(),
        fs::metadata(&built["//pkg:b"]).unwrap().ino()
    );
    // A second build is all cache hits; a damaged tree is rebuilt.
    let again = ws.run(&["build", "//pkg:b"]);
    assert!(
        stderr(&again).contains("0 actions executed, 5 cache hits"),
        "{}",
        stderr(&again)
    );
    let link = tree.join("_main/pkg/d.txt");
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("/elsewhere", &link).unwrap();
    let again = ws.run(&["build", "//pkg:b"]);
    assert!(
        stderr(&again).contains("1 actions executed, 4 cache hits"),
        "{}",
        stderr(&again)
    );
    let manifest = single(&ws, "bazel-out/k8-fastbuild/bin/pkg/b.runfiles/MANIFEST");
    assert_eq!(listing(&ws, manifest.parent().unwrap()).join("\n"), B_TREE);
}

#[test]
fn runfiles_options_select_the_artifacts_and_tree_mode() {
    let ws = Workspace::new();
    // --nobuild_runfile_links: the input manifest and repository mapping,
    // no tree; FilesToRunProvider.runfiles_manifest is the input manifest.
    success(&ws.run(&["build", "--nobuild_runfile_links", "//pkg:tool", "//pkg:m"]));
    assert!(published(&ws, "bazel-out/k8-fastbuild/bin/pkg/tool.runfiles/MANIFEST").is_empty());
    assert_eq!(
        published(&ws, "bazel-out/k8-fastbuild/bin/pkg/tool.runfiles_manifest").len(),
        1
    );
    let m = single(&ws, "bazel-out/k8-fastbuild/bin/pkg/m.txt");
    assert_eq!(
        fs::read_to_string(m).unwrap(),
        "_main/pkg/d.txt\n_main/pkg/tool\n_main/pkg/tool.gen\n_repo_mapping\next+/data/e.txt\n===\n,ext,ext+\n,rf,_main\next+,ext,ext+\n"
    );
    // --enable_runfiles=no: only MANIFEST and the `_main` directory.
    let ws = Workspace::new();
    success(&ws.run(&["build", "--enable_runfiles=no", "//pkg:tool"]));
    let manifest = single(&ws, "bazel-out/k8-fastbuild/bin/pkg/tool.runfiles/MANIFEST");
    assert_eq!(
        listing(&ws, manifest.parent().unwrap()),
        [
            "MANIFEST -> <OUT>/bazel-out/k8-fastbuild/bin/pkg/tool.runfiles_manifest",
            "_main/"
        ]
    );
    // --nobuild_runfile_manifests: only the repository mapping manifest.
    let ws = Workspace::new();
    success(&ws.run(&["build", "--nobuild_runfile_manifests", "//pkg:tool"]));
    assert!(published(&ws, "bazel-out/k8-fastbuild/bin/pkg/tool.runfiles_manifest").is_empty());
    assert_eq!(
        published(&ws, "bazel-out/k8-fastbuild/bin/pkg/tool.repo_mapping").len(),
        1
    );
    let rejected = ws.run(&["run", "--nobuild_runfile_manifests", "//pkg:tool"]);
    assert!(!rejected.status.success());
    assert!(
        stderr(&rejected)
            .contains("--nobuild_runfile_manifests is incompatible with the \"run\" command")
    );
    // Invalid tri-state value: TriStateConverter's error (genuine capture
    // tests/fixtures/enable_runfiles_option bad_value).
    let rejected = ws.run(&["build", "--enable_runfiles=maybe", "//pkg:tool"]);
    assert!(
        stderr(&rejected)
            .contains("While parsing option --enable_runfiles=maybe: 'maybe' is not a boolean"),
        "{}",
        stderr(&rejected)
    );
}

#[test]
fn tool_runfiles_trees_are_staged_into_actions() {
    let ws = Workspace::new();
    let expected = "./\n./_main/\n./_main/pkg/\n./_main/pkg/d.txt: main-data|\n./_main/pkg/tool: #!/bin/sh|echo tool-ran|\n./_main/pkg/tool.gen: generated|\n./_repo_mapping: ,ext,ext+|,rf,_main|ext+,ext,ext+|\n./ext+/\n./ext+/data/\n./ext+/data/e.txt: ext-data|\ntool\ntool.runfiles\n";
    for target in ["use", "use_provider"] {
        let built = outputs(&ws.run(&["build", &format!("//pkg:{target}")]));
        // The tool's directory also holds the action's own output.
        assert_eq!(
            fs::read_to_string(&built[&format!("//pkg:{target}")]).unwrap(),
            format!("{expected}{target}.txt\ntool-ran\n")
        );
    }
    // A runfiles tree merged into another's runfiles keeps only its files.
    success(&ws.run(&["build", "//pkg:outer"]));
    let manifest = single(
        &ws,
        "bazel-out/k8-fastbuild/bin/pkg/outer.runfiles_manifest",
    );
    assert_eq!(
        normalize(&ws, &fs::read_to_string(manifest).unwrap()),
        "_main/pkg/d.txt <WS>/pkg/d.txt\n_main/pkg/outer <OUT>/bazel-out/k8-fastbuild/bin/pkg/outer\n_main/pkg/outer.gen <OUT>/bazel-out/k8-fastbuild/bin/pkg/outer.gen\n_main/pkg/tool <OUT>/bazel-out/k8-fastbuild/bin/pkg/tool\n_main/pkg/tool.gen <OUT>/bazel-out/k8-fastbuild/bin/pkg/tool.gen\n_main/sym/d.txt <WS>/pkg/d.txt\n_repo_mapping <OUT>/bazel-out/k8-fastbuild/bin/pkg/outer.repo_mapping\next+/data/e.txt <WS>/ext/data/e.txt\next+/rooted.txt <OUT>/bazel-out/k8-fastbuild/bin/pkg/outer.gen\nrootsym.txt <WS>/pkg/d.txt\n"
    );
}

/// PA-1: rules whose actions use one tool (`tool`: data, a generated file
/// and an external file; `py`: Python sources with generated empty
/// `__init__.py` runfiles). Each action lists its tool's runfiles: `one`
/// and `two` share the tree as one input group, `inside` also has an input
/// inside the tree (the tool's MANIFEST, so the tree is listed in that
/// action), and `deferred` expands a directory when it executes (its
/// command line is completed then, keeping the empty files).
const SHARED_DEFS: &str = r##"LIST = "(cd $2.runfiles && find . \\( -type f -o -type l \\) | LC_ALL=C sort) > $1; echo \"args=$3\" >> $1"

def _tool(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, "#!/bin/sh\n", is_executable = True)
    gen = ctx.actions.declare_file(ctx.label.name + ".gen")
    ctx.actions.write(gen, "generated\n")
    rf = ctx.runfiles(files = ctx.files.data + [gen])
    if ctx.attr.python:
        rf = py_internal.merge_runfiles_with_generated_inits_empty_files_supplier(ctx = ctx, runfiles = rf)
    return [DefaultInfo(executable = exe, runfiles = rf)]

tool = rule(_tool, executable = True, attrs = {
    "data": attr.label_list(allow_files = True),
    "python": attr.bool(),
})

def _twice(ctx):
    tool = ctx.attr.tool[DefaultInfo].files_to_run
    exe = ctx.executable.tool
    outs = []
    for name in ["one", "two"]:
        out = ctx.actions.declare_file(ctx.label.name + "." + name)
        ctx.actions.run_shell(outputs = [out], tools = [tool], command = LIST, arguments = [out.path, exe.path])
        outs.append(out)
    inside = ctx.actions.declare_file(ctx.label.name + ".inside")
    ctx.actions.run_shell(
        outputs = [inside],
        inputs = [tool.runfiles_manifest],
        tools = [tool],
        command = LIST,
        arguments = [inside.path, exe.path],
    )
    outs.append(inside)
    tree = ctx.actions.declare_directory(ctx.label.name + ".dir")
    ctx.actions.run_shell(outputs = [tree], command = "mkdir -p $1 && echo x > $1/f.txt", arguments = [tree.path])
    deferred = ctx.actions.declare_file(ctx.label.name + ".deferred")
    args = ctx.actions.args()
    args.add(deferred.path)
    args.add(exe.path)
    args.add_all([tree])
    ctx.actions.run_shell(outputs = [deferred], inputs = [tree], tools = [tool], command = LIST, arguments = [args])
    outs.append(deferred)
    return [DefaultInfo(files = depset(outs))]

twice = rule(_twice, attrs = {"tool": attr.label(executable = True, cfg = "target")})
"##;

const SHARED_BUILD: &str = r#"load(":defs.bzl", "tool", "twice")
tool(name = "tool", data = ["d.txt", "@ext//data:fg"])
tool(name = "pytool", data = ["//py:srcs"], python = True)
twice(name = "t", tool = ":tool")
twice(name = "py", tool = ":pytool")
"#;

#[test]
fn actions_sharing_a_tool_list_its_runfiles_tree() {
    let ws = Workspace::new();
    let package = "tools/build_defs/python/shared";
    ws.file(&format!("{package}/defs.bzl"), SHARED_DEFS, 0o644);
    ws.file(&format!("{package}/BUILD.bazel"), SHARED_BUILD, 0o644);
    ws.file(&format!("{package}/d.txt"), "shared-data\n", 0o644);
    let output = ws
        .command(
            &[
                "build",
                &format!("//{package}:t"),
                &format!("//{package}:py"),
            ],
            "",
        )
        .env("BAZEL_RS_DUMP_ACTIONS", "1")
        .env("BAZEL_RS_TIMING", "1")
        .output()
        .unwrap();
    success(&output);
    let text = stderr(&output);
    // Every output, by its name in the package.
    let content = |name: &str| {
        let exec = format!("bazel-out/k8-fastbuild/bin/{package}/{name}");
        fs::read_to_string(single(&ws, &exec)).unwrap()
    };
    let tool_tree = format!(
        "./_main/{package}/d.txt\n./_main/{package}/tool\n./_main/{package}/tool.gen\n./_repo_mapping\n./ext+/data/e.txt\n"
    );
    assert_eq!(content("t.one"), format!("{tool_tree}args=\n"));
    assert_eq!(content("t.two"), format!("{tool_tree}args=\n"));
    assert_eq!(
        content("t.inside"),
        format!("./MANIFEST\n{tool_tree}args=\n")
    );
    assert_eq!(
        content("t.deferred"),
        format!("{tool_tree}args=bazel-out/k8-fastbuild/bin/{package}/t.dir/f.txt\n")
    );
    let py_tree = format!(
        "./_main/py/__init__.py\n./_main/py/__pycache__/c.pyc\n./_main/py/a/__init__.py\n./_main/py/a/b/__init__.py\n./_main/py/a/b/m.py\n./_main/py/a/x.so\n./_main/py/top.py\n./_main/{package}/pytool\n./_main/{package}/pytool.gen\n./_repo_mapping\n"
    );
    assert_eq!(content("py.one"), format!("{py_tree}args=\n"));
    assert_eq!(content("py.two"), format!("{py_tree}args=\n"));
    assert_eq!(
        content("py.deferred"),
        format!("{py_tree}args=bazel-out/k8-fastbuild/bin/{package}/py.dir/f.txt\n")
    );
    // The action dump lists every action's tree entries, each its own.
    let dump = |id: &str| -> Vec<String> {
        let mut lines = text
            .lines()
            .skip_while(|line| *line != format!("ACTION {id}"));
        lines.next().expect(id);
        lines
            .take_while(|line| line.starts_with("  "))
            .filter(|line| line.starts_with("  input ") || line.starts_with("  generated "))
            .map(str::to_owned)
            .collect()
    };
    let (one, two) = (
        dump(&format!("//{package}:t#0")),
        dump(&format!("//{package}:t#1")),
    );
    assert_eq!(one, two);
    let tree = format!("bazel-out/k8-fastbuild/bin/{package}/tool.runfiles");
    for entry in [
        format!("<WS>/{package}/d.txt -> {tree}/_main/{package}/d.txt"),
        format!("<WS>/ext/data/e.txt -> {tree}/ext+/data/e.txt"),
        format!("generated //{package}:tool#1[0] -> {tree}/_main/{package}/tool.gen"),
        format!("generated //{package}:tool#repo_mapping[0] -> {tree}/_repo_mapping"),
    ] {
        assert!(
            one.iter()
                .any(|line| normalize(&ws, line).trim().ends_with(&entry)),
            "{entry}: {one:?}"
        );
    }
    let inside = dump(&format!("//{package}:t#2"));
    assert!(
        inside
            .iter()
            .any(|line| line.ends_with(&format!("-> {tree}/MANIFEST"))),
        "{inside:?}"
    );
    assert!(one.iter().all(|line| inside.contains(line)), "{inside:?}");
    // Two shared trees (the tools'), held by the 2 x 3 actions that need no
    // listing; the `inside` actions list theirs.
    assert!(
        text.contains("holders=6 refs=6 distinct=2 ") && text.contains(" listed_tree_actions=2"),
        "{}",
        text.lines()
            .filter(|line| line.starts_with("PERF-GROUPS"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// PA-1 follow-up B6: an aspect's actions stage their tools' runfiles trees
/// and complete deferred command lines like a rule's (the aspect commit
/// used to drop both: the tool found no `$0.runfiles`, the deferred command
/// line ran without arguments). The aspect runs two tools (`tool`: data, a
/// generated file and an external file; `pytool`: Python sources with
/// generated empty `__init__.py` files) as `executable`, as a shell tool,
/// with the tool's MANIFEST as an input (the tree is listed in that
/// action) and with a command line expanding a directory. The expected
/// outputs are genuine Bazel 9.3.0's for this package over the fixture
/// above (`--batch --ignore_all_rc_files`, byte-identical).
const ASPECT_DEFS: &str = r##"SCRIPT = """#!/bin/sh
out=$1
shift
{ (cd $0.runfiles && find . \\( -type f -o -type l \\) | LC_ALL=C sort); echo "args=$*"; } > $out
"""

def _tool(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, SCRIPT, is_executable = True)
    gen = ctx.actions.declare_file(ctx.label.name + ".gen")
    ctx.actions.write(gen, "generated\n")
    rf = ctx.runfiles(files = ctx.files.data + [gen])
    if ctx.attr.python:
        rf = py_internal.merge_runfiles_with_generated_inits_empty_files_supplier(ctx = ctx, runfiles = rf)
    return [DefaultInfo(executable = exe, runfiles = rf)]

tool = rule(_tool, executable = True, attrs = {
    "data": attr.label_list(allow_files = True),
    "python": attr.bool(),
})

def _base(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".base")
    ctx.actions.write(out, "base\n")
    return [DefaultInfo(files = depset([out]))]

base = rule(_base)

def _use_tools(target, ctx):
    outs = []
    for attribute in [ctx.attr._tool, ctx.attr._pytool]:
        tool = attribute[DefaultInfo].files_to_run
        prefix = target.label.name + "." + tool.executable.basename
        run = ctx.actions.declare_file(prefix + ".run")
        ctx.actions.run(outputs = [run], executable = tool, arguments = [run.path])
        shell = ctx.actions.declare_file(prefix + ".shell")
        ctx.actions.run_shell(outputs = [shell], tools = [tool], command = "$1 $2", arguments = [tool.executable.path, shell.path])
        inside = ctx.actions.declare_file(prefix + ".inside")
        ctx.actions.run(outputs = [inside], inputs = [tool.runfiles_manifest], executable = tool, arguments = [inside.path])
        tree = ctx.actions.declare_directory(prefix + ".dir")
        ctx.actions.run_shell(outputs = [tree], command = "mkdir -p $1 && echo x > $1/f.txt", arguments = [tree.path])
        deferred = ctx.actions.declare_file(prefix + ".deferred")
        args = ctx.actions.args()
        args.add(deferred.path)
        args.add_all([tree])
        ctx.actions.run(outputs = [deferred], inputs = [tree], executable = tool, arguments = [args])
        outs += [run, shell, inside, deferred]
    return [OutputGroupInfo(tool_out = depset(outs))]

use_tools = aspect(_use_tools, attrs = {
    "_tool": attr.label(default = Label("//tools/build_defs/python/aspect:tool"), executable = True, cfg = "exec"),
    "_pytool": attr.label(default = Label("//tools/build_defs/python/aspect:pytool"), executable = True, cfg = "exec"),
})
"##;

const ASPECT_BUILD: &str = r#"load(":defs.bzl", "base", "tool")
tool(name = "tool", data = ["d.txt", "@ext//data:fg"])
tool(name = "pytool", data = ["//py:srcs"], python = True)
base(name = "b")
"#;

#[test]
fn aspect_actions_stage_their_tools_runfiles() {
    let ws = Workspace::new();
    let package = "tools/build_defs/python/aspect";
    ws.file(&format!("{package}/defs.bzl"), ASPECT_DEFS, 0o644);
    ws.file(&format!("{package}/BUILD.bazel"), ASPECT_BUILD, 0o644);
    ws.file(&format!("{package}/d.txt"), "aspect-data\n", 0o644);
    let output = ws
        .command(
            &[
                "build",
                &format!("--aspects=//{package}:defs.bzl%use_tools"),
                "--output_groups=tool_out",
                &format!("//{package}:b"),
            ],
            "",
        )
        .env("BAZEL_RS_TIMING", "1")
        .output()
        .unwrap();
    success(&output);
    let content = |name: &str| {
        let exec = format!("bazel-out/k8-fastbuild/bin/{package}/{name}");
        fs::read_to_string(single(&ws, &exec)).unwrap()
    };
    let tool_tree = format!(
        "./_main/{package}/d.txt\n./_main/{package}/tool\n./_main/{package}/tool.gen\n./_repo_mapping\n./ext+/data/e.txt\n"
    );
    let py_tree = format!(
        "./_main/py/__init__.py\n./_main/py/__pycache__/c.pyc\n./_main/py/a/__init__.py\n./_main/py/a/b/__init__.py\n./_main/py/a/b/m.py\n./_main/py/a/x.so\n./_main/py/top.py\n./_main/{package}/pytool\n./_main/{package}/pytool.gen\n./_repo_mapping\n"
    );
    for (tool, tree) in [("tool", &tool_tree), ("pytool", &py_tree)] {
        assert_eq!(content(&format!("b.{tool}.run")), format!("{tree}args=\n"));
        assert_eq!(
            content(&format!("b.{tool}.shell")),
            format!("{tree}args=\n")
        );
        assert_eq!(
            content(&format!("b.{tool}.inside")),
            format!("./MANIFEST\n{tree}args=\n")
        );
        assert_eq!(
            content(&format!("b.{tool}.deferred")),
            format!("{tree}args=bazel-out/k8-fastbuild/bin/{package}/b.{tool}.dir/f.txt\n")
        );
    }
    // The run, shell and deferred actions hold their tool's shared group;
    // the `inside` actions list their tool's tree.
    let text = stderr(&output);
    assert!(
        text.contains("holders=6 refs=6 distinct=2 ") && text.contains(" listed_tree_actions=2"),
        "{}",
        text.lines()
            .filter(|line| line.starts_with("PERF-GROUPS"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// PA-1 follow-up B6 F1 (integration-21): an aspect attached to an
/// attribute (`aspects = [...]`, planned while its consumer's prerequisites
/// are: the PlanState Engine::prepare builds for an aspect-applied
/// dependency) and propagating further through `attr_aspects` runs the
/// tool its base targets' rule actions run (`ctx.rule.attr.tool`): one
/// FilesToRunProvider, so one runfiles tree and ONE shared input group for
/// the two rule actions and the two aspect actions — the aspect commits use
/// the build's group memo, not one of their own.
const ATTR_ASPECT_DEFS: &str = r##"SCRIPT = """#!/bin/sh
{ (cd $0.runfiles && find . \\( -type f -o -type l \\) | LC_ALL=C sort); echo "out=$(basename $1)"; } > $1
"""

def _tool(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, SCRIPT, is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

tool = rule(_tool, executable = True, attrs = {"data": attr.label_list(allow_files = True)})

def _base(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".base")
    ctx.actions.run(outputs = [out], executable = ctx.executable.tool, arguments = [out.path])
    return [DefaultInfo(files = depset([out], transitive = [dep[DefaultInfo].files for dep in ctx.attr.deps]))]

base = rule(_base, attrs = {
    "tool": attr.label(executable = True, cfg = "target"),
    "deps": attr.label_list(),
})

def _via_tool(target, ctx):
    out = ctx.actions.declare_file(target.label.name + ".aspect")
    ctx.actions.run(outputs = [out], executable = ctx.rule.attr.tool[DefaultInfo].files_to_run, arguments = [out.path])
    return [OutputGroupInfo(aspect_out = depset([out], transitive = [dep[OutputGroupInfo].aspect_out for dep in ctx.rule.attr.deps]))]

via_tool = aspect(_via_tool, attr_aspects = ["deps"])

def _top(ctx):
    return [DefaultInfo(files = depset(transitive = [dep[OutputGroupInfo].aspect_out for dep in ctx.attr.deps]))]

top = rule(_top, attrs = {"deps": attr.label_list(aspects = [via_tool])})
"##;

const ATTR_ASPECT_BUILD: &str = r#"load(":defs.bzl", "base", "tool", "top")
tool(name = "tool", data = ["d.txt"])
base(name = "c", tool = ":tool")
base(name = "b", tool = ":tool", deps = [":c"])
top(name = "t", deps = [":b"])
"#;

#[test]
fn attribute_aspects_share_the_tool_group_of_rule_actions() {
    let ws = Workspace::new();
    let package = "attr_aspect";
    ws.file(&format!("{package}/defs.bzl"), ATTR_ASPECT_DEFS, 0o644);
    ws.file(&format!("{package}/BUILD.bazel"), ATTR_ASPECT_BUILD, 0o644);
    ws.file(&format!("{package}/d.txt"), "tool-data\n", 0o644);
    let output = ws
        .command(
            &[
                "build",
                &format!("//{package}:t"),
                &format!("//{package}:b"),
            ],
            "",
        )
        .env("BAZEL_RS_TIMING", "1")
        .env("BAZEL_RS_DUMP_ACTIONS", "1")
        .output()
        .unwrap();
    success(&output);
    let tree = format!("./_main/{package}/d.txt\n./_main/{package}/tool\n./_repo_mapping\n");
    for name in ["b.base", "c.base", "b.aspect", "c.aspect"] {
        let exec = format!("bazel-out/k8-fastbuild/bin/{package}/{name}");
        assert_eq!(
            fs::read_to_string(single(&ws, &exec)).unwrap(),
            format!("{tree}out={name}\n"),
            "{name}"
        );
    }
    // Four holders of one group: the aspect actions found the rule
    // actions' group in the build's memo.
    let text = stderr(&output);
    assert!(
        text.contains("holders=4 refs=4 distinct=1 ") && text.contains(" listed_tree_actions=0"),
        "{}",
        text.lines()
            .filter(|line| line.starts_with("PERF-GROUPS"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The dump lists the same tree entries for a rule's and an aspect's action.
    let dump = |id_end: &str| -> Vec<String> {
        let mut lines = text
            .lines()
            .skip_while(|line| !(line.starts_with("ACTION ") && line.ends_with(id_end)));
        lines.next().expect(id_end);
        lines
            .take_while(|line| line.starts_with("  "))
            .filter(|line| line.contains(".runfiles/"))
            .map(str::to_owned)
            .collect()
    };
    let (rule, aspect) = (dump(&format!("//{package}:b#0")), dump("via_tool#0"));
    assert!(!rule.is_empty());
    assert_eq!(rule, aspect);
}

#[test]
fn python_empty_files_and_obscured_symlinks() {
    let ws = Workspace::new();
    let result = ws.run(&["build", "//tools/build_defs/python:p", "//pkg:conflict"]);
    success(&result);
    // PythonUtils.GetInitPyFiles in Java HashSet order; __pycache__ needs none.
    assert!(
        stderr(&result)
            .contains("EMPTY [\"py/a/b/__init__.py\",\"py/a/__init__.py\",\"py/__init__.py\"]"),
        "{}",
        stderr(&result)
    );
    let manifest = single(
        &ws,
        "bazel-out/k8-fastbuild/bin/tools/build_defs/python/p.runfiles/MANIFEST",
    );
    let tree = listing(&ws, manifest.parent().unwrap());
    assert!(
        tree.contains(&"_main/py/__init__.py (empty)".to_owned()),
        "{tree:?}"
    );
    assert!(
        tree.contains(&"_main/py/a/b/__init__.py (empty)".to_owned()),
        "{tree:?}"
    );
    let input = fs::read_link(&manifest).unwrap();
    let text = fs::read_to_string(input).unwrap();
    assert!(
        text.starts_with("_main/py/__init__.py \n_main/py/__pycache__/c.pyc "),
        "{text}"
    );
    // The obscured `dir/x` is dropped and reported at the rule's location
    // (ConflictPolicy.ERROR: explicit symlinks).
    assert!(
        stderr(&result).contains(
            "pkg/BUILD.bazel:13:9: runfiles symlink dir/x -> pkg/d.txt obscured by dir -> pkg/d.txt"
        ),
        "{}",
        stderr(&result)
    );
    let manifest = single(
        &ws,
        "bazel-out/k8-fastbuild/bin/pkg/conflict.runfiles_manifest",
    );
    assert_eq!(
        normalize(&ws, &fs::read_to_string(manifest).unwrap()),
        "_main/dir <WS>/pkg/d.txt\n_main/pkg/conflict <OUT>/bazel-out/k8-fastbuild/bin/pkg/conflict\n_repo_mapping <OUT>/bazel-out/k8-fastbuild/bin/pkg/conflict.repo_mapping\n"
    );
}

#[test]
fn run_executes_the_target_in_its_runfiles_directory() {
    let ws = Workspace::new();
    let mut child = ws
        .command(
            &[
                "run",
                "--run_env=RUN_SET=1",
                "--run_env==CLIENT_UNSET",
                "//pkg:b",
                "--",
                "x y",
                "z",
            ],
            "pkg",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"from-stdin\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let workspace = ws.path().display().to_string();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "argv0=bazel-out/k8-fastbuild/bin/pkg/b\nargs=fixed x y z\ncwd=bazel-out/k8-fastbuild/bin/pkg/b.runfiles/_main\nBUILD_WORKING_DIRECTORY={workspace}/pkg\nBUILD_WORKSPACE_DIRECTORY={workspace}\nCLIENT_X=1\nFROM_TARGET=yes\nINHERIT_ME=inherited\nRUN_SET=1\npkg\nsym\nmain-data\next-data\nMANIFEST\n_main\n_repo_mapping\next+\nrootsym.txt\nstdin=from-stdin\n"
        )
    );
    assert!(
        stderr(&output).contains("bazel-rs: Running command line: "),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("/bin/pkg/b fixed <args omitted>"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn run_script_path_writes_the_script() {
    let ws = Workspace::new();
    success(&ws.run_in(
        &["run", "--script_path=run.sh", "//pkg:b", "--", "arg 1"],
        "pkg",
    ));
    let script_path = ws.path().join("pkg/run.sh");
    assert_eq!(
        fs::metadata(&script_path).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let script = fs::read_to_string(&script_path).unwrap();
    let lines: Vec<&str> = script.lines().collect();
    assert_eq!(lines[0], "#!/bin/bash");
    assert!(
        lines[1].starts_with("cd /")
            && lines[1].ends_with("/bazel-out/k8-fastbuild/bin/pkg/b.runfiles/_main && \\")
    );
    assert_eq!(
        &lines[2..8],
        [
            "  exec env \\",
            "    -u JAVA_RUNFILES \\",
            "    -u RUNFILES_DIR \\",
            "    -u RUNFILES_MANIFEST_FILE \\",
            "    -u RUNFILES_MANIFEST_ONLY \\",
            "    -u TEST_SRCDIR \\",
        ]
    );
    assert!(lines[8].starts_with("    BUILD_EXECROOT=/"));
    assert!(lines[9].starts_with("    BUILD_ID="));
    let workspace = ws.path().display().to_string();
    assert_eq!(
        lines[10],
        format!("    BUILD_WORKING_DIRECTORY={workspace}/pkg \\")
    );
    assert_eq!(
        lines[11],
        format!("    BUILD_WORKSPACE_DIRECTORY={workspace} \\")
    );
    assert_eq!(lines[12], "    FROM_TARGET=yes \\");
    assert_eq!(lines[13], "    INHERIT_ME=inherited \\");
    assert!(lines[14].ends_with("/bazel-out/k8-fastbuild/bin/pkg/b fixed 'arg 1' \"$@\""));
    assert!(!script.ends_with('\n'));
    // The script runs the target like `run` (stdin from /dev/null here).
    let output = Command::new(&script_path)
        .arg("more")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stdout).contains("args=fixed arg 1 more\n"));
    // --run_under wraps the command in the shell.
    success(&ws.run_in(
        &[
            "run",
            "--run_under=env FOO=1",
            "--script_path=under.sh",
            "//pkg:b",
        ],
        "pkg",
    ));
    let script = fs::read_to_string(ws.path().join("pkg/under.sh")).unwrap();
    let last = script.lines().last().unwrap();
    assert!(
        last.starts_with("  /bin/bash -c 'env FOO=1 /")
            && last.ends_with("/bin/pkg/b fixed' \"$@\""),
        "{last}"
    );
}

#[test]
fn run_validates_its_target() {
    let ws = Workspace::new();
    for (args, message) in [
        (vec!["run"], "Must specify a target to run"),
        (
            vec!["run", "//pkg:group"],
            "Cannot run target //pkg:group: Not executable",
        ),
        (
            vec!["run", "//pkg:all"],
            "Only a single target can be run. Your target pattern //pkg:all expanded to the targets",
        ),
        (
            vec!["run", "--run_under=//pkg:tool", "//pkg:b"],
            "a label prefix (LabelRunUnder) is not supported",
        ),
        // --run_env converts with N42's EnvVarsConverter (crate::action_env).
        (
            vec!["run", "--run_env=", "//pkg:b"],
            "While parsing option --run_env=: Variable definitions must be in the form of a 'name=value', 'name', or '=name' assignment",
        ),
    ] {
        let output = ws.run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(
            stderr(&output).contains(message),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    // A source file target runs in the working directory, without runfiles.
    let output = ws.run_in(&["run", "//pkg:script.sh", "--", "a"], "pkg");
    success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("source-script a\n{}/pkg\n", ws.path().display())
    );
    // --norun builds and prints the command line only.
    let output = ws.run(&["run", "--norun", "//pkg:tool"]);
    success(&output);
    assert!(output.stdout.is_empty());
    assert!(
        stderr(&output).contains("bazel-rs: Runnable command line: "),
        "{}",
        stderr(&output)
    );
}

/// Extra packages of the post-review tests (not part of the paired capture
/// fixture): a host binary copied as an executable, and a target analyzed in
/// two configurations whose select() picks a dependency from `ext` in one.
const PKG2_DEFS: &str = r##"load("//pkg:defs.bzl", "my_bin")

def _copy(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.run_shell(outputs = [exe], command = "/bin/cp /usr/bin/readlink \"$1\"", arguments = [exe.path])
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

copy_exe = rule(_copy, executable = True, attrs = {"data": attr.label_list(allow_files = True)})

def _to_opt(settings, attr):
    return {"//command_line_option:compilation_mode": "opt"}

to_opt = transition(implementation = _to_opt, inputs = [], outputs = ["//command_line_option:compilation_mode"])

def _pair(ctx):
    files = []
    for dep in [ctx.attr.a_opt[0], ctx.attr.b_plain]:
        files.append(dep[DefaultInfo].files_to_run.repo_mapping_manifest)
    return [DefaultInfo(files = depset(files))]

pair = rule(_pair, attrs = {
    "a_opt": attr.label(cfg = to_opt),
    "b_plain": attr.label(),
    "_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist"),
})
"##;

const PKG2_BUILD: &str = r##"load(":defs.bzl", "copy_exe", "pair")
load("//pkg:defs.bzl", "my_bin")
copy_exe(name = "readlink", data = ["d2.txt"])
my_bin(
    name = "real",
    data = ["d2.txt"],
    script = "#!/bin/bash\nr=$(realpath \"$0\")\necho \"$r\"\ncat \"$r.runfiles/_main/pkg2/d2.txt\"\n",
)
config_setting(name = "opt", values = {"compilation_mode": "opt"})
my_bin(
    name = "selected",
    data = ["d2.txt"],
    deps = select({":opt": ["@ext//empty"], "//conditions:default": []}),
)
pair(name = "pair", a_opt = ":selected", b_plain = ":selected")
"##;

fn add_pkg2(ws: &Workspace) {
    ws.file("pkg2/defs.bzl", PKG2_DEFS, 0o644);
    ws.file("pkg2/BUILD.bazel", PKG2_BUILD, 0o644);
    ws.file("pkg2/d2.txt", "second-data\n", 0o644);
    ws.file(
        "ext/empty/BUILD.bazel",
        "filegroup(name = \"empty\", visibility = [\"//visibility:public\"])\n",
        0o644,
    );
    ws.file(
        "tools/allowlists/function_transition_allowlist/BUILD.bazel",
        "package_group(name = \"function_transition_allowlist\", packages = [\"public\"])\n",
        0o644,
    );
}

#[test]
fn executable_sibling_resolves_proc_self_exe_and_realpath_to_its_runfiles() {
    use std::os::unix::fs::MetadataExt;
    let ws = Workspace::new();
    add_pkg2(&ws);
    // A real ELF program: /proc/self/exe names the path it was executed
    // through (Go's os.Executable(), canonicalizing runfiles libraries).
    let output = ws.run(&["run", "//pkg2:readlink", "--", "-f", "/proc/self/exe"]);
    success(&output);
    let exe = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    assert!(
        exe.ends_with("bazel-out/k8-fastbuild/bin/pkg2/readlink"),
        "{}",
        exe.display()
    );
    assert!(exe.starts_with(ws.cache().join("actions")));
    let runfiles = exe.with_file_name("readlink.runfiles");
    assert_eq!(
        fs::read_to_string(runfiles.join("_main/pkg2/d2.txt")).unwrap(),
        "second-data\n"
    );
    // One inode: the published executable, its store blob and the sibling.
    let sibling = fs::symlink_metadata(&exe).unwrap();
    assert!(sibling.is_file());
    let published = published_without_tree(&ws, "bazel-out/k8-fastbuild/bin/pkg2/readlink");
    assert_eq!(fs::metadata(published).unwrap().ino(), sibling.ino());
    assert!(sibling.nlink() >= 3);
    // A script resolving realpath($0) also lands next to its runfiles.
    let output = ws.run(&["run", "//pkg2:real"]);
    success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut lines = stdout.lines();
    let real = PathBuf::from(lines.next().unwrap());
    assert!(
        real.ends_with("bazel-out/k8-fastbuild/bin/pkg2/real"),
        "{stdout}"
    );
    assert!(real.with_file_name("real.runfiles").is_dir());
    assert_eq!(lines.next(), Some("second-data"));
}

/// The executable output itself (the entry without a `.runfiles` sibling).
fn published_without_tree(ws: &Workspace, exec: &str) -> PathBuf {
    published(ws, exec)
        .into_iter()
        .find(|path| {
            !path
                .with_file_name(format!(
                    "{}.runfiles",
                    path.file_name().unwrap().to_string_lossy()
                ))
                .exists()
        })
        .unwrap()
}

#[test]
fn repo_mapping_source_repositories_follow_each_configuration() {
    let ws = Workspace::new();
    add_pkg2(&ws);
    let output = ws.run(&["build", "//pkg2:pair"]);
    success(&output);
    let built: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| PathBuf::from(line.split_once('\t').unwrap().1))
        .collect();
    assert_eq!(built.len(), 2, "{built:?}");
    let mut contents: Vec<(String, String)> = built
        .iter()
        .map(|path| {
            let shown = path.to_string_lossy();
            let config = shown
                .split("/bazel-out/")
                .nth(1)
                .unwrap()
                .split('/')
                .next()
                .unwrap()
                .to_owned();
            (config, fs::read_to_string(path).unwrap())
        })
        .collect();
    contents.sort();
    // Only the opt configuration's select() depends on a package of ext+,
    // so only its manifest has ext+'s own line; ext+ is in both runfiles
    // (the `ext+/rooted.txt` root symlink).
    let (plain, opt): (Vec<_>, Vec<_>) = contents
        .into_iter()
        .partition(|(config, _)| config == "k8-fastbuild");
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].1, ",ext,ext+\n,rf,_main\n");
    assert_eq!(opt.len(), 1);
    assert!(opt[0].0.starts_with("k8-opt"), "{}", opt[0].0);
    assert_eq!(opt[0].1, ",ext,ext+\n,rf,_main\next+,ext,ext+\n");
}

#[test]
fn query_never_lists_runfiles_artifacts() {
    // Runfiles trees, manifests and repo mappings are analysis-time
    // artifacts of RunfilesSupport, not package targets: Bazel's query
    // (and the native N39 query) lists only the rules and files of the
    // package, also after a build created them.
    let ws = Workspace::new();
    success(&ws.run(&["build", "//pkg:b", "//pkg:use"]));
    for pattern in ["//pkg:*", "//pkg:all", "deps(//pkg:use)"] {
        let output = ws.run(&["query", pattern]);
        success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.is_empty(), "{pattern}");
        for marker in [
            ".runfiles",
            ".repo_mapping",
            "runfiles_manifest",
            "MANIFEST",
        ] {
            assert!(
                !stdout.contains(marker),
                "{pattern} lists {marker}:\n{stdout}"
            );
        }
    }
}

/// Writes a fixture file with `mode`; an executable one (`bazel run` execs
/// source scripts through the runfiles tree) is written by a separate process
/// so that no descriptor of it exists in this spawning test process (ETXTBSY).
fn write_mode(path: &Path, text: &str, mode: u32) {
    if mode & 0o111 != 0 {
        write_executable_script(path, text, mode);
    } else {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
}
