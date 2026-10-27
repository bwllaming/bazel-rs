//! N23a CLI coverage: runfiles values, DefaultInfo/FilesToRunProvider views,
//! executable and test rules (analysis only), docs/RUNFILES.md.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");
include!("support/console.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("runfiles-cli-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='runfiles_fixture')\n");
        for (name, text) in files {
            ws.file(name, text);
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }
}

fn errors(output: &Output) -> String {
    without_print_locations(&String::from_utf8_lossy(&output.stderr))
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", errors(output));
}

fn rejected(output: &Output, diagnostic: &str) {
    assert!(!output.status.success(), "unexpected success");
    assert!(output.stdout.is_empty(), "partial output on failure");
    assert!(errors(output).contains(diagnostic), "{}", errors(output));
    assert!(!errors(output).contains("panicked"), "{}", errors(output));
}

fn outputs(output: &Output) -> Vec<(String, PathBuf)> {
    success(output);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (label, path) = line.split_once('\t').unwrap();
            (label.to_owned(), PathBuf::from(path))
        })
        .collect()
}

/// `print("PROBE " + json.encode(...))` lines, decoded.
fn probes(output: &Output) -> Vec<serde_json::Value> {
    errors(output)
        .lines()
        .filter_map(|line| line.strip_prefix("PROBE "))
        .map(|json| serde_json::from_str(json).unwrap())
        .collect()
}

const PROBE_DEFS: &str = r##"
def _sp(files):
    return [f.short_path for f in files]

def _rf(r):
    return {
        "files": _sp(r.files.to_list()),
        "symlinks": [[s.path, s.target_file.short_path] for s in r.symlinks.to_list()],
        "root_symlinks": [[s.path, s.target_file.short_path] for s in r.root_symlinks.to_list()],
        "empty": r.empty_filenames.to_list(),
    }

def _ftr(f):
    return {
        "repr": repr(f),
        "type": type(f),
        "executable": f.executable.short_path if f.executable else None,
        "manifest": f.runfiles_manifest.short_path if f.runfiles_manifest else None,
        "repo_mapping": f.repo_mapping_manifest.short_path if f.repo_mapping_manifest else None,
    }

def _view(target):
    info = target[DefaultInfo]
    return {
        "files": _sp(info.files.to_list()),
        "default": _rf(info.default_runfiles),
        "data": _rf(info.data_runfiles),
        "files_to_run": _ftr(info.files_to_run),
        "dir": dir(info),
    }

def _lib(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.label.name)
    runfiles = ctx.runfiles(files = [out], collect_data = ctx.attr.collect_data, collect_default = ctx.attr.collect_default)
    if ctx.attr.stateful:
        return [DefaultInfo(files = depset([out]), data_runfiles = runfiles, default_runfiles = ctx.runfiles(files = ctx.files.srcs))]
    return [DefaultInfo(files = depset([out]), runfiles = runfiles)]

lib = rule(_lib, attrs = {
    "srcs": attr.label_list(allow_files = True),
    "deps": attr.label_list(),
    "data": attr.label_list(allow_files = True),
    "collect_data": attr.bool(),
    "collect_default": attr.bool(),
    "stateful": attr.bool(),
})

def _probe(ctx):
    a = ctx.file.src
    empty = ctx.runfiles()
    r = ctx.runfiles(
        files = [a],
        transitive_files = depset([a], order = "postorder"),
        symlinks = {"x/./y//z": a, "q/../w": a},
        root_symlinks = {"root/r": a},
    )
    s = ctx.runfiles(symlinks = r.symlinks, root_symlinks = depset(r.root_symlinks.to_list()))
    collected = ctx.runfiles(collect_data = True)
    defaults = ctx.runfiles(collect_default = True)
    print("PROBE " + json.encode({
        "probe": _rf(r),
        "type": type(r),
        "repr": repr(r),
        "symlink_type": type(r.symlinks.to_list()[0]),
        "symlink_repr": repr(r.symlinks.to_list()[0]).startswith('SymlinkEntry(path = "x/y/z", target_file = '),
        "dir": dir(r),
        "merge_identity": [
            empty.merge(r) == r,
            r.merge(empty) == r,
            r.merge_all([empty, empty]) == r,
            empty.merge_all([r]) == r,
            empty.merge_all([empty, r, empty]) == r,
            r.merge(s) == r,
            empty.merge_all([]) == empty,
            ctx.runfiles() == ctx.runfiles(),
            r == r,
        ],
        "merged": _rf(r.merge_all([s, collected])),
        "copied_symlinks": _rf(s),
        "collect_data": _rf(collected),
        "collect_default": _rf(defaults),
        "deps": {str(d.label): _view(d) for d in ctx.attr.deps + ctx.attr.data},
        "alias": [str(ctx.attr.deps[-1].label), _view(ctx.attr.deps[-1])],
        "fresh": [repr(DefaultInfo()), repr(DefaultInfo(runfiles = r).files_to_run), DefaultInfo(runfiles = r).default_runfiles == r, DefaultInfo(data_runfiles = r).default_runfiles == None, DefaultInfo(default_runfiles = r).data_runfiles == None],
    }))
    return []

probe = rule(_probe, attrs = {
    "src": attr.label(allow_single_file = True),
    "deps": attr.label_list(),
    "data": attr.label_list(allow_files = True),
})
"##;

const PROBE_BUILD: &str = r##"
load(":defs.bzl", "lib", "probe")
genrule(name = "gen", outs = ["gen.out"], cmd = "echo gen > $@")
filegroup(name = "group", srcs = ["a.txt", ":gen", ":leaf"])
filegroup(name = "single", srcs = ["b.txt"])
alias(name = "leaf_alias", actual = ":leaf")
lib(name = "leaf", srcs = ["b.txt"], data = ["a.txt", ":gen"], collect_default = True)
lib(name = "stateful", srcs = ["a.txt"], deps = [":leaf"], collect_data = True, stateful = True)
probe(name = "probe", src = "a.txt", deps = [":leaf", ":stateful", ":group", ":single", ":gen", ":leaf_alias"], data = ["b.txt", ":gen", ":group", ":stateful"])
"##;

#[test]
fn runfiles_values_merge_identity_and_dependency_views() {
    let ws = Workspace::new(&[
        ("pkg/defs.bzl", PROBE_DEFS),
        ("pkg/BUILD.bazel", PROBE_BUILD),
        ("pkg/a.txt", "a\n"),
        ("pkg/b.txt", "b\n"),
    ]);
    let result = ws.run(&["build", "//pkg:probe"]);
    success(&result);
    let probes = probes(&result);
    assert_eq!(probes.len(), 1, "{}", errors(&result));
    let probe = &probes[0];
    let rf = |files: &[&str], symlinks: &[[&str; 2]], roots: &[[&str; 2]]| serde_json::json!({"files": files, "symlinks": symlinks, "root_symlinks": roots, "empty": []});
    let empty = rf(&[], &[], &[]);
    // Normalized PathFragments, dict order, depset (STABLE) symlinks.
    assert_eq!(
        probe["probe"],
        rf(
            &["pkg/a.txt"],
            &[["x/y/z", "pkg/a.txt"], ["w", "pkg/a.txt"]],
            &[["root/r", "pkg/a.txt"]]
        )
    );
    assert_eq!(probe["type"], "runfiles");
    assert_eq!(
        probe["repr"],
        "<unknown object com.google.devtools.build.lib.analysis.Runfiles>"
    );
    assert_eq!(probe["symlink_type"], "SymlinkEntry");
    assert_eq!(probe["symlink_repr"], true);
    assert_eq!(
        probe["dir"],
        serde_json::json!([
            "empty_filenames",
            "files",
            "merge",
            "merge_all",
            "root_symlinks",
            "symlinks"
        ])
    );
    assert_eq!(
        probe["merge_identity"],
        // merge_all of nothing returns Runfiles.EMPTY, not the receiver.
        serde_json::json!([true, true, true, true, true, false, false, false, true])
    );
    assert_eq!(
        probe["copied_symlinks"]["symlinks"],
        probe["probe"]["symlinks"]
    );
    // data: files to build (always_include_files_in_data) then data runfiles;
    // file targets contribute only themselves.
    assert_eq!(
        probe["collect_data"],
        rf(
            &[
                "pkg/b.txt",
                "pkg/gen.out",
                "pkg/a.txt",
                "pkg/leaf.txt",
                "pkg/stateful.txt"
            ],
            &[],
            &[]
        )
    );
    // deps use the default runfiles (a stateful default_runfiles is `srcs`).
    assert_eq!(
        probe["collect_default"],
        rf(
            &[
                "pkg/b.txt",
                "pkg/gen.out",
                "pkg/a.txt",
                "pkg/leaf.txt",
                "pkg/stateful.txt"
            ],
            &[],
            &[]
        )
    );
    let deps = &probe["deps"];
    let leaf = rf(&["pkg/a.txt", "pkg/gen.out", "pkg/leaf.txt"], &[], &[]);
    let base = "<unknown object com.google.devtools.build.lib.analysis.FilesToRunProvider>";
    let plain = serde_json::json!({"repr": base, "type": "FilesToRunProvider", "executable": null, "manifest": null, "repo_mapping": null});
    let dir = serde_json::json!(["data_runfiles", "default_runfiles", "files", "files_to_run"]);
    assert_eq!(
        deps["@@//pkg:leaf"],
        serde_json::json!({"files": ["pkg/leaf.txt"], "default": leaf, "data": leaf, "files_to_run": plain, "dir": dir})
    );
    // An alias forwards the actual's label and providers.
    assert_eq!(
        probe["alias"],
        serde_json::json!(["@@//pkg:leaf", deps["@@//pkg:leaf"]])
    );
    assert_eq!(
        deps["@@//pkg:stateful"]["default"],
        rf(&["pkg/a.txt"], &[], &[])
    );
    assert_eq!(
        deps["@@//pkg:stateful"]["data"],
        rf(
            &[
                "pkg/a.txt",
                "pkg/gen.out",
                "pkg/leaf.txt",
                "pkg/stateful.txt"
            ],
            &[],
            &[]
        )
    );
    // genrule: no default runfiles, outputs as data runfiles.
    assert_eq!(deps["@@//pkg:gen"]["default"], empty);
    assert_eq!(deps["@@//pkg:gen"]["data"], rf(&["pkg/gen.out"], &[], &[]));
    assert_eq!(deps["@@//pkg:gen"]["files_to_run"], plain);
    // filegroup: srcs' default runfiles; data = files to build + srcs' data.
    assert_eq!(
        deps["@@//pkg:group"]["default"],
        rf(&["pkg/a.txt", "pkg/gen.out", "pkg/leaf.txt"], &[], &[])
    );
    assert_eq!(
        deps["@@//pkg:group"]["data"],
        rf(&["pkg/a.txt", "pkg/gen.out", "pkg/leaf.txt"], &[], &[])
    );
    let single = "<unknown object com.google.devtools.build.lib.analysis.FilesToRunProvider$SingleExecutableFilesToRunProvider>";
    assert_eq!(
        deps["@@//pkg:single"]["files_to_run"],
        serde_json::json!({"repr": single, "type": "FilesToRunProvider", "executable": "pkg/b.txt", "manifest": null, "repo_mapping": null})
    );
    // Input file targets: no RunfilesProvider, a single executable.
    assert_eq!(deps["@@//pkg:b.txt"]["default"], empty);
    assert_eq!(deps["@@//pkg:b.txt"]["data"], empty);
    assert_eq!(
        deps["@@//pkg:b.txt"]["files_to_run"]["repr"],
        serde_json::json!(single)
    );
    assert_eq!(
        probe["fresh"],
        serde_json::json!([
            "struct(data_runfiles = None, default_runfiles = None, files = None, files_to_run = None)",
            "None",
            true,
            true,
            true
        ])
    );
}

const EXECUTABLE_DEFS: &str = r##"
def _sp(files):
    return [f.short_path for f in files]

def _bin(ctx):
    exe = ctx.outputs.executable if ctx.attr.lazy else ctx.actions.declare_file(ctx.label.name + ".sh")
    print("PROBE " + json.encode({"dir": dir(ctx.outputs), "name": ctx.label.name}))
    ctx.actions.write(exe, "#!/bin/sh\necho " + ctx.label.name + "\n", is_executable = True)
    runfiles = ctx.runfiles(collect_data = True)
    if ctx.attr.lazy:
        return [DefaultInfo(runfiles = runfiles)]
    return [DefaultInfo(executable = exe, runfiles = runfiles), RunEnvironmentInfo({"K": "v"}, ["HOME"])]

def _attrs():
    return {"data": attr.label_list(allow_files = True), "lazy": attr.bool()}

my_binary = rule(_bin, executable = True, attrs = _attrs())
my_test = rule(_bin, test = True, attrs = _attrs())

def _use(ctx):
    info = ctx.attr.tool[DefaultInfo]
    f = info.files_to_run
    print("PROBE " + json.encode({
        "executable": ctx.executable.tool.short_path if ctx.executable.tool else None,
        "none": ctx.executable.none,
        "files": _sp(info.files.to_list()),
        "default": _sp(info.default_runfiles.files.to_list()),
        "data": _sp(info.data_runfiles.files.to_list()),
        "ftr": [repr(f), f.executable.short_path, f.runfiles_manifest.short_path, f.runfiles_manifest.path.endswith("/pkg/" + f.executable.basename + ".runfiles/MANIFEST"), f.repo_mapping_manifest.short_path],
        "dir": dir(ctx.executable),
    }))
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "used")
    return [DefaultInfo(files = depset([out]))]

use = rule(_use, attrs = {
    "tool": attr.label(executable = True, cfg = "target"),
    "none": attr.label(executable = True, cfg = "target"),
})
"##;

#[test]
fn executable_and_test_rules_build_their_executable_and_hidden_runfiles() {
    let ws = Workspace::new(&[
        ("pkg/defs.bzl", EXECUTABLE_DEFS),
        (
            "pkg/BUILD.bazel",
            r##"
load(":defs.bzl", "my_binary", "my_test", "use")
genrule(name = "gen", outs = ["gen.out"], cmd = "echo gen > $@")
my_binary(name = "bin", data = ["a.txt", ":gen"], args = ["--flag", "plain words"])
my_binary(name = "lazy", lazy = True)
my_test(name = "unit_test", data = ["a.txt"])
use(name = "use", tool = ":bin")
"##,
        ),
        ("pkg/a.txt", "a\n"),
    ]);
    let result = ws.run(&["build", "//pkg:bin", "//pkg:lazy", "//pkg:unit_test"]);
    let built = outputs(&result);
    // Bazel's --show_result order is its own; compare as a set.
    let mut names = built
        .iter()
        .map(|(label, path)| {
            (
                label.as_str(),
                path.file_name().unwrap().to_string_lossy().into_owned(),
            )
        })
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        [
            ("//pkg:bin", "bin.sh".to_owned()),
            ("//pkg:lazy", "lazy".to_owned()),
            ("//pkg:unit_test", "unit_test.sh".to_owned()),
        ]
    );
    let bin = &built
        .iter()
        .find(|(label, _)| label == "//pkg:bin")
        .unwrap()
        .1;
    assert_eq!(fs::read_to_string(bin).unwrap(), "#!/bin/sh\necho bin\n");
    // The data genrule is a default runfile of //pkg:bin: built (hidden
    // top-level output group) but not reported.
    // N23f: plus the repository mapping manifest, runfiles manifest and
    // runfiles tree of each of the three executables.
    assert!(
        errors(&result).contains("13 actions executed"),
        "{}",
        errors(&result)
    );
    // dir(ctx.outputs) shows `executable` only after access.
    let dirs = probes(&result);
    assert!(dirs.contains(&serde_json::json!({"dir": [], "name": "bin"})));
    assert!(dirs.contains(&serde_json::json!({"dir": ["executable"], "name": "lazy"})));

    let result = ws.run(&["build", "//pkg:use"]);
    success(&result);
    let probes = probes(&result);
    let probe = probes
        .iter()
        .find(|probe| probe.get("executable").is_some())
        .unwrap();
    assert_eq!(probe["executable"], "pkg/bin.sh");
    assert_eq!(probe["none"], serde_json::Value::Null);
    assert_eq!(probe["dir"], serde_json::json!(["none", "tool"]));
    assert_eq!(probe["files"], serde_json::json!(["pkg/bin.sh"]));
    // The executable is merged into the runfiles (COMPILE order puts the
    // direct executable after the transitive runfiles).
    assert_eq!(
        probe["default"],
        serde_json::json!(["pkg/a.txt", "pkg/gen.out", "pkg/bin.sh"])
    );
    assert_eq!(probe["data"], probe["default"]);
    assert_eq!(
        probe["ftr"],
        serde_json::json!([
            "<unknown object com.google.devtools.build.lib.analysis.FilesToRunProvider$FullFilesToRunProvider>",
            "pkg/bin.sh",
            "pkg/bin.sh.runfiles/MANIFEST",
            true,
            "pkg/bin.sh.repo_mapping"
        ])
    );
}

#[test]
fn executable_default_info_and_runfiles_errors_match_bazel() {
    let defs = r##"
def _none(ctx):
    return []
noexe = rule(_none, executable = True)
notest_test = rule(_none, test = True)
def _both(ctx):
    ctx.actions.write(ctx.outputs.executable, "x", is_executable = True)
    other = ctx.actions.declare_file("other")
    ctx.actions.write(other, "y", is_executable = True)
    return [DefaultInfo(executable = other)]
both = rule(_both, executable = True)
def _foreign(ctx):
    return [DefaultInfo(executable = ctx.file.src)]
foreign = rule(_foreign, executable = True, attrs = {"src": attr.label(allow_single_file = True)})
def _bad(ctx):
    m = ctx.attr.mode
    a = ctx.file.src
    if m == "order":
        ctx.runfiles(transitive_files = depset(order = "preorder"))
    elif m == "topological":
        ctx.runfiles(transitive_files = depset([a], order = "topological"))
    elif m == "files":
        ctx.runfiles(files = ["x"])
    elif m == "files_type":
        ctx.runfiles(files = depset([a]))
    elif m == "transitive":
        ctx.runfiles(transitive_files = depset(["x"]))
    elif m == "transitive_type":
        ctx.runfiles(transitive_files = [a])
    elif m == "symlinks":
        ctx.runfiles(symlinks = {"k": "v"})
    elif m == "symlinks_type":
        ctx.runfiles(root_symlinks = [a])
    elif m == "symlinks_depset":
        ctx.runfiles(symlinks = depset([a]))
    elif m == "collect_type":
        ctx.runfiles(collect_data = 1)
    elif m == "skip":
        ctx.runfiles(skip_conflict_checking = True)
    elif m == "stateless":
        DefaultInfo(runfiles = ctx.runfiles(), data_runfiles = ctx.runfiles())
    elif m == "merge":
        ctx.runfiles().merge([])
    elif m == "merge_all":
        ctx.runfiles().merge_all([ctx.runfiles(), 1])
    elif m == "merge_all_type":
        ctx.runfiles().merge_all(ctx.runfiles())
    elif m == "default_files":
        return [DefaultInfo(files = depset(["x"]))]
    elif m == "manifest":
        # Bazel builds FilesToRunProvider manifests; generating them is N23f.
        return [DefaultInfo(runfiles = ctx.runfiles(files = [ctx.attr.tool[DefaultInfo].files_to_run.runfiles_manifest]))]
    elif m == "run_env":
        return [RunEnvironmentInfo({}, [])]
    return []
bad = rule(_bad, attrs = {"mode": attr.string(), "src": attr.label(allow_single_file = True), "tool": attr.label()})
def _use(ctx):
    return []
use = rule(_use, attrs = {"tool": attr.label(executable = True, cfg = "target", allow_files = True)})
hidden_use = rule(_use, attrs = {"_hidden": attr.label(executable = True, cfg = "target", default = "//pkg:group")})
def _run(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    if ctx.attr.provider:
        ctx.actions.run(executable = ctx.attr.tool[DefaultInfo].files_to_run, outputs = [out])
    else:
        ctx.actions.run(executable = ctx.executable.tool, outputs = [out], arguments = [out.path])
    return [DefaultInfo(files = depset([out]))]
run = rule(_run, attrs = {"tool": attr.label(executable = True, cfg = "target", allow_files = True), "provider": attr.bool()})
def _lazy(ctx):
    ctx.actions.write(ctx.outputs.executable, "#!/bin/sh\n", is_executable = True)
    return []
lazy = rule(_lazy, executable = True)
def _out(ctx):
    return []
outexe = rule(_out, executable = True, attrs = {"executable": attr.output()})
"##;
    let modes = [
        ("order", "order 'preorder' is invalid for transitive_files"),
        (
            "topological",
            "order 'topological' is invalid for transitive_files",
        ),
        (
            "files",
            "at index 0 of files, got element of type string, want File",
        ),
        (
            "files_type",
            "in call to runfiles(), parameter 'files' got value of type 'depset', want 'sequence'",
        ),
        (
            "transitive",
            "for 'transitive_files', got a depset of 'string', expected a depset of 'File'",
        ),
        (
            "transitive_type",
            "in call to runfiles(), parameter 'transitive_files' got value of type 'list', want 'depset or NoneType'",
        ),
        (
            "symlinks",
            "got dict<string, string> for 'symlinks', want dict<string, File>",
        ),
        (
            "symlinks_type",
            "in call to runfiles(), parameter 'root_symlinks' got value of type 'list', want 'dict or depset'",
        ),
        (
            "symlinks_depset",
            "got a depset of 'File', expected a depset of 'SymlinkEntry'",
        ),
        (
            "collect_type",
            "in call to runfiles(), parameter 'collect_data' got value of type 'int', want 'bool'",
        ),
        ("skip", "file '//pkg:defs.bzl' cannot use private API"),
        (
            "stateless",
            "Cannot specify the provider 'runfiles' together with 'data_runfiles' or 'default_runfiles'",
        ),
        (
            "merge",
            "in call to merge(), parameter 'other' got value of type 'list', want 'runfiles'",
        ),
        (
            "merge_all",
            "at index 1 of param, got element of type int, want runfiles",
        ),
        (
            "merge_all_type",
            "in call to merge_all(), parameter 'other' got value of type 'runfiles', want 'sequence'",
        ),
        (
            "default_files",
            "for 'files', got a depset of 'string', expected a depset of 'File'",
        ),
        (
            "run_env",
            "Returning RunEnvironmentInfo from a non-executable, non-test target has no effect",
        ),
    ];
    let mut build = String::from(
        "load(':defs.bzl', 'bad', 'noexe', 'notest_test', 'both', 'foreign', 'use', 'hidden_use', 'run', 'lazy', 'outexe')\n\
         filegroup(name = 'group', srcs = ['a.txt', 'b.txt'])\n\
         noexe(name = 'noexe')\nnotest_test(name = 'notest_test')\nboth(name = 'both')\n\
         foreign(name = 'foreign', src = 'a.txt')\nuse(name = 'use_group', tool = ':group')\n\
         hidden_use(name = 'use_hidden')\nlazy(name = 'lazy')\n\
         run(name = 'run_lazy', tool = ':lazy')\nrun(name = 'run_provider', tool = ':lazy', provider = True)\n\
         run(name = 'run_source', tool = 'tool.sh')\noutexe(name = 'outexe', executable = 'e.out')\n\
         bad(name = 'manifest', mode = 'manifest', src = 'a.txt', tool = ':lazy')\n",
    );
    for (mode, _) in modes {
        build.push_str(&format!(
            "bad(name = '{mode}', mode = '{mode}', src = 'a.txt')\n"
        ));
    }
    let ws = Workspace::new(&[
        ("pkg/defs.bzl", defs),
        ("pkg/BUILD.bazel", &build),
        ("pkg/a.txt", "a\n"),
        ("pkg/b.txt", "b\n"),
        ("pkg/tool.sh", "#!/bin/sh\ntouch \"$1\"\n"),
    ]);
    for (mode, message) in modes {
        rejected(&ws.run(&["build", &format!("//pkg:{mode}")]), message);
    }
    // Like genuine Bazel, the runfiles manifest is a generated file (N23f).
    success(&ws.run(&["build", "//pkg:manifest"]));
    for (target, message) in [
        (
            "noexe",
            "The rule 'noexe' is executable. It needs to create an executable File and pass it as the 'executable' parameter to the DefaultInfo it returns.",
        ),
        (
            "notest_test",
            "The rule 'notest_test' is executable. It needs to create an executable File",
        ),
        (
            "both",
            "The rule 'both' both accesses 'ctx.outputs.executable' and provides a different executable 'pkg/other'. Do not use 'ctx.output.executable'.",
        ),
        (
            "foreign",
            "'executable' provided by an executable rule 'foreign' should be created by the same rule.",
        ),
        (
            "use_group",
            "in tool attribute of use rule //pkg:use_group: //pkg:group does not refer to a valid executable target",
        ),
        (
            "use_hidden",
            "in (an implicit dependency) attribute of hidden_use rule //pkg:use_hidden: //pkg:group does not refer to a valid executable target",
        ),
        // Like Bazel, the tool's runfiles tree is attached (N23f); the
        // build then fails because //pkg:lazy writes no output.
        (
            "run_lazy",
            "has missing or invalid output 'bazel-out/k8-fastbuild/bin/pkg/run_lazy.out'",
        ),
        (
            "run_provider",
            "has missing or invalid output 'bazel-out/k8-fastbuild/bin/pkg/run_provider.out'",
        ),
        (
            "outexe",
            "in outexe rule //pkg:outexe: Multiple outputs with the same key: executable",
        ),
    ] {
        rejected(&ws.run(&["build", &format!("//pkg:{target}")]), message);
    }
    // A source file has a single-executable FilesToRunProvider without runfiles.
    // Re-created by a separate process (ETXTBSY: no descriptor of the
    // executable in this spawning test process).
    let tool = ws.path().join("pkg/tool.sh");
    write_executable_script(&tool, fs::read(&tool).unwrap(), 0o755);
    let built = outputs(&ws.run(&["build", "//pkg:run_source"]));
    assert_eq!(built.len(), 1);
    // `args` expansion (RunfilesSupport.computeArgs, tests/test_args.rs):
    // Bazel rejects this one because :group is not a prerequisite.
    ws.file(
        "pkg/BUILD.bazel",
        &format!("{build}lazy(name = 'args', args = ['$(location :group)'])\nlazy(name = 'plain_args', args = ['a', 'b c'])\n"),
    );
    rejected(
        &ws.run(&["build", "//pkg:args"]),
        "in args attribute of lazy rule //pkg:args: label '//pkg:group' in $(location) expression is not a declared prerequisite of this rule",
    );
    success(&ws.run(&["build", "//pkg:plain_args"]));
}

/// rules_shell's sh_library/sh_binary/sh_test shape (shell/private/sh_library.bzl,
/// sh_executable.bzl): ctx.runfiles(transitive_files=..., collect_default=True)
/// and DefaultInfo(executable=..., files=..., runfiles=...).
const SHELL_DEFS: &str = r##"
def _sh_library_impl(ctx):
    transitive_files = []
    for target in ctx.attr.srcs + ctx.attr.deps + ctx.attr.data:
        transitive_files.append(target[DefaultInfo].files)
    files = depset(transitive = transitive_files)
    runfiles = ctx.runfiles(transitive_files = files, collect_default = True)
    return [DefaultInfo(files = files, runfiles = runfiles)]

sh_library = rule(_sh_library_impl, attrs = {
    "srcs": attr.label_list(allow_files = True),
    "deps": attr.label_list(),
    "data": attr.label_list(allow_files = True),
})

def _sh_executable_impl(ctx):
    if len(ctx.files.srcs) != 1:
        fail("you must specify exactly one file in 'srcs'", attr = "srcs")
    symlink = ctx.actions.declare_file(ctx.label.name)
    src = ctx.files.srcs[0]
    if ctx.attr.use_symlink:
        ctx.actions.symlink(output = symlink, target_file = src, is_executable = True)
    else:
        ctx.actions.run_shell(outputs = [symlink], inputs = [src], command = "cp \"$1\" \"$2\" && chmod +x \"$2\"", arguments = [src.path, symlink.path])
    direct_files = [src, symlink]
    files = depset(direct = direct_files)
    runfiles = ctx.runfiles(transitive_files = files, collect_default = True)
    default_info = DefaultInfo(executable = symlink, files = files, runfiles = runfiles)
    run_environment_info = RunEnvironmentInfo(environment = ctx.attr.env, inherited_environment = ctx.attr.env_inherit)
    print("PROBE " + json.encode({"name": ctx.label.name, "runfiles": [f.short_path for f in runfiles.files.to_list()]}))
    return [default_info, run_environment_info]

def _executable_attrs():
    return {
        "srcs": attr.label_list(allow_files = True),
        "data": attr.label_list(allow_files = True),
        "deps": attr.label_list(),
        "env": attr.string_dict(),
        "env_inherit": attr.string_list(),
        "use_symlink": attr.bool(),
    }

sh_binary = rule(_sh_executable_impl, executable = True, attrs = _executable_attrs())
sh_test = rule(_sh_executable_impl, test = True, attrs = _executable_attrs())

def _consumer(ctx):
    info = ctx.attr.binary[DefaultInfo]
    print("PROBE " + json.encode({
        "files": [f.short_path for f in info.files.to_list()],
        "default": [f.short_path for f in info.default_runfiles.files.to_list()],
        "executable": info.files_to_run.executable.short_path,
        "manifest": info.files_to_run.runfiles_manifest.short_path,
    }))
    return []

consumer = rule(_consumer, attrs = {"binary": attr.label()})
"##;

#[test]
fn rules_shell_shaped_binary_library_and_test() {
    let ws = Workspace::new(&[
        ("shell/defs.bzl", SHELL_DEFS),
        (
            "shell/BUILD.bazel",
            r##"
load(":defs.bzl", "consumer", "sh_binary", "sh_library", "sh_test")
sh_library(name = "lib", srcs = ["lib.sh"], data = ["data.txt"])
sh_binary(name = "tool", srcs = ["tool.sh"], deps = [":lib"], data = ["data.txt"], env = {"A": "b"}, env_inherit = ["HOME"])
sh_test(name = "tool_test", srcs = ["tool.sh"], deps = [":lib"])
sh_binary(name = "linked", srcs = ["tool.sh"], use_symlink = True)
consumer(name = "consumer", binary = ":tool")
"##,
        ),
        ("shell/lib.sh", "echo lib\n"),
        ("shell/tool.sh", "#!/bin/sh\necho tool\n"),
        ("shell/data.txt", "data\n"),
    ]);
    let result = ws.run(&[
        "build",
        "//shell:tool",
        "//shell:tool_test",
        "//shell:consumer",
    ]);
    let built = outputs(&result);
    // Generated outputs only: the native CLI also lists source files to build
    // (shell/tool.sh), which Bazel's --show_result omits (pre-existing CLI
    // difference, docs/RUNFILES.md).
    let mut names = built
        .iter()
        .filter(|(_, path)| path.starts_with(ws.path().join("private-cache")))
        .map(|(label, path)| format!("{label} {}", path.file_name().unwrap().to_string_lossy()))
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["//shell:tool tool", "//shell:tool_test tool_test"]);
    let tool = built
        .iter()
        .find(|(label, path)| label == "//shell:tool" && path.ends_with("tool"))
        .unwrap();
    assert_eq!(
        fs::read_to_string(&tool.1).unwrap(),
        "#!/bin/sh\necho tool\n"
    );
    let probes = probes(&result);
    // collect_default merges before transitive_files; COMPILE order lists
    // transitive sets in insertion order: data, the library's runfiles
    // ([data.txt, lib.sh]), then the binary's own files.
    assert!(probes.contains(&serde_json::json!({
        "name": "tool",
        "runfiles": ["shell/data.txt", "shell/lib.sh", "shell/tool.sh", "shell/tool"]
    })));
    assert!(probes.contains(&serde_json::json!({
        "name": "tool_test",
        "runfiles": ["shell/data.txt", "shell/lib.sh", "shell/tool.sh", "shell/tool_test"]
    })));
    assert!(probes.contains(&serde_json::json!({
        "files": ["shell/tool.sh", "shell/tool"],
        "default": ["shell/data.txt", "shell/lib.sh", "shell/tool.sh", "shell/tool"],
        "executable": "shell/tool",
        "manifest": "shell/tool.runfiles/MANIFEST"
    })));
    // ctx.actions.symlink(is_executable=True) checks the target's owner
    // execute bit (SymlinkAction.java:296-330); the output observes the
    // target's bytes and permission bits (docs/ACTIONS_PARAMS.md).
    rejected(
        &ws.run(&["build", "//shell:linked"]),
        "file 'shell/tool.sh' is not executable",
    );
    // Re-created by a separate process (ETXTBSY: no descriptor of the
    // executable in this spawning test process).
    let tool = ws.path().join("shell/tool.sh");
    write_executable_script(&tool, fs::read(&tool).unwrap(), 0o755);
    let linked = outputs(&ws.run(&["build", "//shell:linked"]));
    let (_, path) = linked
        .iter()
        .find(|(_, path)| path.ends_with("shell/linked"))
        .unwrap();
    assert_eq!(fs::read(path).unwrap(), b"#!/bin/sh\necho tool\n");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o7777,
        0o755
    );
}

/// Bazel 9.3 messages and identities captured by the N23a paired capture
/// (tests/fixtures/runfiles/reference_receipt.json), and run/run_shell tools.
const BAZEL_DEFS: &str = r##"
def _show(ctx):
    m = ctx.attr.mode
    if m == "struct":
        return struct(files = depset())
    if m == "two":
        return [DefaultInfo(), DefaultInfo()]
    if m == "setfield":
        r = ctx.runfiles()
        r.files = depset()
    if m == "outputs_executable":
        return [ctx.outputs.executable]
    if m == "executable_missing":
        return [ctx.executable.nope]
    if m == "values":
        file_info = ctx.attr.src[DefaultInfo]
        rule_info = ctx.attr.dep[DefaultInfo]
        print("PROBE " + json.encode([
            DefaultInfo() == DefaultInfo(),
            type(ctx.outputs),
            file_info.files_to_run == file_info.files_to_run,
            rule_info.files_to_run == rule_info.files_to_run,
        ]))
    return []

show = rule(_show, attrs = {
    "mode": attr.string(),
    "src": attr.label(allow_single_file = True),
    "dep": attr.label(),
    "tool": attr.label(executable = True, cfg = "target", allow_files = True),
})

def _no_action(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    return [DefaultInfo(executable = exe)]

no_action = rule(_no_action, executable = True)

def _bin(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\ntouch \"$1\"\n", is_executable = True)
    return [DefaultInfo(executable = exe)]

binary = rule(_bin, executable = True)

def _tools(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    tool = ctx.attr.tool[DefaultInfo].files_to_run
    tools = depset([ctx.executable.tool]) if ctx.attr.depset else [tool]
    ctx.actions.run_shell(tools = tools, outputs = [out], command = "$1 $2", arguments = [ctx.executable.tool.path, out.path])
    return [DefaultInfo(files = depset([out]))]

tools = rule(_tools, attrs = {"tool": attr.label(executable = True, cfg = "target", allow_files = True), "depset": attr.bool()})
"##;

#[test]
fn bazel_messages_identities_and_action_tools() {
    let ws = Workspace::new(&[
        ("pkg/defs.bzl", BAZEL_DEFS),
        (
            "pkg/BUILD.bazel",
            r#"
load(":defs.bzl", "binary", "no_action", "show", "tools")
genrule(name = "gen", outs = ["gen.out"], cmd = "echo gen > $@")
[show(name = m, mode = m, tool = "tool.sh") for m in ["struct", "two", "setfield", "outputs_executable", "executable_missing"]]
show(name = "values", mode = "values", src = "tool.sh", dep = ":gen")
no_action(name = "no_action")
binary(name = "bin")
tools(name = "source_tool", tool = "tool.sh")
tools(name = "depset_tool", tool = "tool.sh", depset = True)
tools(name = "binary_tool", tool = ":bin")
"#,
        ),
        ("pkg/tool.sh", "#!/bin/sh\ntouch \"$1\"\n"),
    ]);
    for (target, message) in [
        (
            "struct",
            "Returning a struct from a rule implementation function is deprecated.",
        ),
        (
            "two",
            "Multiple conflicting returned providers with key DefaultInfo",
        ),
        ("setfield", "cannot set .files field of runfiles value"),
        (
            "outputs_executable",
            "No attribute 'executable' in outputs. Make sure you declared a rule output with this name.",
        ),
        (
            "executable_missing",
            "No attribute 'nope' in executable. Make sure there is a label type attribute marked as 'executable' with this name\nAvailable attributes: tool",
        ),
        (
            "no_action",
            "The following files have no generating action:\npkg/no_action.sh",
        ),
    ] {
        rejected(&ws.run(&["build", &format!("//pkg:{target}")]), message);
    }
    // N23f: the tool's runfiles tree is staged into the spawn.
    success(&ws.run(&["build", "//pkg:binary_tool"]));
    let result = ws.run(&["build", "//pkg:values"]);
    success(&result);
    // DefaultInfo() == DefaultInfo() (NativeInfo value equality); every rule
    // has an `Outputs` ctx.outputs; a file target's FilesToRunProvider is a
    // new object per access, a rule target's is one object.
    assert_eq!(
        probes(&result),
        [serde_json::json!([true, "Outputs", false, true])]
    );
    // Re-created by a separate process (ETXTBSY: no descriptor of the
    // executable in this spawning test process).
    let tool = ws.path().join("pkg/tool.sh");
    write_executable_script(&tool, fs::read(&tool).unwrap(), 0o755);
    for target in ["//pkg:source_tool", "//pkg:depset_tool"] {
        let built = outputs(&ws.run(&["build", target]));
        assert_eq!(built.len(), 1, "{target}");
        assert_eq!(fs::read(&built[0].1).unwrap(), b"", "{target}");
    }
}

/// Runfiles and FilesToRunProvider debugPrint (Runfiles.java:935-945,
/// FilesToRunProvider.java:112-120) with the N22 hook; repr stays the
/// StarlarkValue default. An executable attribute with cfg="exec" is the
/// exec-configured dependency's executable, below its exec output root.
#[test]
fn runfiles_debug_print_and_exec_configured_executables() {
    let ws = Workspace::new(&[
        (
            "defs.bzl",
            r##"def _tool(ctx):
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(out, "#!/bin/sh\n", is_executable = True)
    return [DefaultInfo(executable = out)]
tool = rule(implementation = _tool, executable = True)
def _check(ctx):
    f = ctx.file.src
    r = ctx.runfiles(files = [f], symlinks = {"a/b": f}, root_symlinks = {"r": f})
    print("RF", r)
    print("RFREPR", repr(r))
    print("FTR", ctx.attr._tool[DefaultInfo].files_to_run)
    print("SYM", r.symlinks.to_list()[0])
    out = ctx.actions.declare_file("check.out")
    ctx.actions.write(out, ctx.executable._tool.path + "\n")
    return [DefaultInfo(files = depset([out]))]
check = rule(implementation = _check, attrs = {
    "src": attr.label(allow_single_file = True),
    "_tool": attr.label(default = ":tool", cfg = "exec", executable = True),
})
"##,
        ),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'check', 'tool')\nplatform(name = 'host')\ntool(name = 'tool')\ncheck(name = 'check', src = 'src.txt')\n",
        ),
        ("src.txt", "s\n"),
    ]);
    let output = ws.run(&["build", "--host_platform=//:host", "//:check"]);
    let stderr = errors(&output);
    let built = outputs(&output);
    for line in [
        "RF Runfiles(empty_files = depset([]), files = depset([<source file src.txt>], order = \"postorder\"), root_symlinks = depset([SymlinkEntry(path = \"r\", target_file = <source file src.txt>)]), symlinks = depset([SymlinkEntry(path = \"a/b\", target_file = <source file src.txt>)]))\n",
        "RFREPR <unknown object com.google.devtools.build.lib.analysis.Runfiles>\n",
        "FTR FilesToRunProvider(executable = <generated file tool>, repo_mapping_manifest = <generated file tool.repo_mapping>, runfiles_manifest = <generated file tool.runfiles/MANIFEST>)\n",
        "SYM SymlinkEntry(path = \"a/b\", target_file = <source file src.txt>)\n",
    ] {
        assert!(stderr.contains(line), "{line}\n{stderr}");
    }
    assert_eq!(built.len(), 1);
    assert_eq!(
        fs::read_to_string(&built[0].1).unwrap(),
        "bazel-out/k8-opt-exec/bin/tool\n"
    );
}
