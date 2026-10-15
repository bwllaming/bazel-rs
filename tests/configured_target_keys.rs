//! N18a (explicit configuration plumbing, docs/CONFIGURATION.md
//! "Configured-target keys and output roots"): keying analysis by
//! configured-target keys and taking output roots from the target's
//! configuration changes no visible behavior. The action cache keys of these
//! builds are golden values recorded with the pre-N18a binary (main 040d055):
//! equal keys mean that warm caches of existing builds stay valid. Action
//! keys frame the host OS and architecture (src/action.rs), so the golden
//! values are those of linux/x86_64.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("n18a-")
                .tempdir_in(base)
                .unwrap(),
        );
        for (path, content) in files {
            let path = ws.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn build(&self, cache: &str, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={cache}"))
            .arg("build")
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// The action keys of the cache (`CACHE/actions/KEY`), sorted.
    fn keys(&self, cache: &str) -> Vec<String> {
        let mut keys = fs::read_dir(self.path().join(cache).join("actions"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    /// `LABEL\tOUTPUT` lines with the cache's action directory as `ACTIONS`.
    fn outputs(&self, cache: &str, output: &Output) -> Vec<String> {
        let actions = fs::canonicalize(self.path().join(cache).join("actions")).unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.replace(&actions.display().to_string(), "ACTIONS"))
            .collect()
    }
}

fn summary(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .last()
        .unwrap_or_default()
        .to_owned()
}

/// genrules, a Starlark rule (declare_file with a sibling, write with
/// json.encode of Files, run_shell with Args), a toolchain-using rule
/// asking ctx.target_platform_has_constraint, and a select() on the
/// compilation mode.
const FIXTURE: &[(&str, &str)] = &[
    (
        "MODULE.bazel",
        "module(name = \"main\")\nregister_toolchains(\"//tc:all\")\n",
    ),
    (
        "os/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
constraint_setting(name = "os")
constraint_value(name = "linux", constraint_setting = ":os")
constraint_value(name = "osx", constraint_setting = ":os")
"#,
    ),
    (
        "p/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
platform(name = "linux", constraint_values = ["//os:linux"])
platform(name = "mac", constraint_values = ["//os:osx"])
"#,
    ),
    ("r/BUILD.bazel", ""),
    (
        "r/defs.bzl",
        r#"def _impl_impl(ctx):
    return [platform_common.ToolchainInfo(name = ctx.attr.tool_name, tool = ctx.file.tool), DefaultInfo()]

impl = rule(implementation = _impl_impl, attrs = {"tool_name": attr.string(), "tool": attr.label(allow_single_file = True)})

def _use_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    info = ctx.toolchains["//tc:type"]
    inputs = [info.tool] if info.tool else []
    ctx.actions.run_shell(
        outputs = [out],
        inputs = inputs,
        command = "echo name=%s tool=%s linux=%s > %s" % (info.name, info.tool.path if info.tool else None, ctx.target_platform_has_constraint(ctx.attr._linux[platform_common.ConstraintValueInfo]), out.path),
    )
    return [DefaultInfo(files = depset([out]))]

use = rule(implementation = _use_impl, toolchains = ["//tc:type"], attrs = {"_linux": attr.label(default = "//os:linux")})

def _gen_impl(ctx):
    a = ctx.actions.declare_file(ctx.label.name + ".a.txt")
    b = ctx.actions.declare_file("b.txt", sibling = a)
    ctx.actions.write(a, json.encode({"dep": ctx.file.dep, "mode": ctx.attr.mode, "a": a}) + "\n")
    args = ctx.actions.args()
    args.add(a)
    args.add(ctx.file.dep)
    args.add(b)
    ctx.actions.run_shell(
        outputs = [b],
        inputs = [a, ctx.file.dep],
        arguments = [args],
        command = "cat $1 $2 > $3; echo $1 $2 $3 %s >> $3" % b.dirname,
    )
    return [DefaultInfo(files = depset([a, b]))]

gen = rule(implementation = _gen_impl, attrs = {"dep": attr.label(allow_single_file = True), "mode": attr.string()})

def _consume_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    files = depset(transitive = [d[DefaultInfo].files for d in ctx.attr.deps]).to_list()
    ctx.actions.run_shell(outputs = [out], inputs = files, command = "cat %s > %s" % (" ".join([f.path for f in files]), out.path))
    return [DefaultInfo(files = depset([out]))]

consume = rule(implementation = _consume_impl, attrs = {"deps": attr.label_list()})
"#,
    ),
    (
        "tc/BUILD.bazel",
        r#"load("//r:defs.bzl", "impl")
package(default_visibility = ["//visibility:public"])
toolchain_type(name = "type")
impl(name = "linux_impl", tool_name = "linux", tool = "tool.sh")
toolchain(name = "linux", toolchain_type = ":type", exec_compatible_with = ["//os:linux"], target_compatible_with = ["//os:linux"], toolchain = ":linux_impl")
"#,
    ),
    ("tc/tool.sh", "#!/bin/sh\n"),
    (
        "g/BUILD.bazel",
        r#"load("//r:defs.bzl", "consume", "gen", "use")
config_setting(name = "dbg", values = {"compilation_mode": "dbg"})
genrule(name = "src", outs = ["src.txt"], cmd = "echo src > $@")
gen(name = "gen", dep = ":src", mode = select({":dbg": "dbg", "//conditions:default": "other"}))
genrule(name = "final", srcs = [":gen"], outs = ["final.txt"], cmd = "cat $(SRCS) > $@; echo $(locations :gen) $(RULEDIR) >> $@")
use(name = "u")
consume(name = "c", deps = [":gen", ":u"])
"#,
    ),
];

#[test]
fn action_keys_and_output_roots_match_the_pre_n18a_binary() {
    let ws = Workspace::new(FIXTURE);
    let targets = ["--host_platform=//p:linux", "//g:final", "//g:c", "//g:u"];
    let cold = ws.build("default-cache", &targets);
    assert_eq!(
        ws.keys("default-cache"),
        [
            "15141e806b30397e3393947015a1d6e32b303bb067c294c713ae4627cddfcca7",
            "1fda1aa0324ddf2d085d8986b96f0b3ef159620e942c2a6941e47cdc343b8084",
            "740b95552d53eeed22ce5f061580452d8ed2e6080ffc3030effb94b6e8206b25",
            "ca668fc4c15a181572fe29ec28ffd844540d4434d85168d0642097c26a01f42a",
            "e7f9951295dd31f728ce3f592ad932505647b0faf1ea4cea62b56f520e6c0bec",
            "f1c31acc1f83836061f8cbea2591c26218e20f44f284aaf9610674b5e695f80f",
        ]
    );
    assert_eq!(
        ws.outputs("default-cache", &cold),
        [
            "//g:c\tACTIONS/f1c31acc1f83836061f8cbea2591c26218e20f44f284aaf9610674b5e695f80f/outputs/bazel-out/rust-fastbuild/bin/g/c.txt",
            "//g:final\tACTIONS/e7f9951295dd31f728ce3f592ad932505647b0faf1ea4cea62b56f520e6c0bec/outputs/bazel-out/rust-fastbuild/bin/g/final.txt",
            "//g:u\tACTIONS/ca668fc4c15a181572fe29ec28ffd844540d4434d85168d0642097c26a01f42a/outputs/bazel-out/rust-fastbuild/bin/g/u.txt",
        ]
    );
    assert!(summary(&cold).contains("6 actions executed, 0 cache hits"));
    let final_txt = fs::read_to_string(
        fs::read_dir(ws.path().join("default-cache/actions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.ends_with("e7f9951295dd31f728ce3f592ad932505647b0faf1ea4cea62b56f520e6c0bec")
            })
            .unwrap()
            .join("outputs/bazel-out/rust-fastbuild/bin/g/final.txt"),
    )
    .unwrap();
    assert!(final_txt.starts_with(r#"{"a":{"path":"bazel-out/rust-fastbuild/bin/g/gen.a.txt","root":"bazel-out/rust-fastbuild/bin","short_path":"g/gen.a.txt"}"#), "{final_txt}");
    assert!(final_txt.ends_with("bazel-out/rust-fastbuild/bin/g/b.txt bazel-out/rust-fastbuild/bin/g/gen.a.txt bazel-out/rust-fastbuild/bin/g\n"), "{final_txt}");
    // A warm build hits every key.
    let warm = ws.build("default-cache", &targets);
    assert!(summary(&warm).contains("0 actions executed, 6 cache hits"));
    // -c dbg keeps its distinct root and its own keys.
    let dbg = ws.build(
        "dbg-cache",
        &[
            "-c",
            "dbg",
            "--host_platform=//p:linux",
            "//g:final",
            "//g:c",
        ],
    );
    assert_eq!(
        ws.keys("dbg-cache"),
        [
            "150b8fe9961dcbc8932b4390175c6d828e9fbf348921ec11b13235d6a15f3897",
            "72396987f01b239fe5a9b98b073c329ca30dbdcf7fa8600b917f26acc9a39af9",
            "72598e33a3484a6ece9c52dfe645af70c21c4e82a0daa929e35c1e649fe00dcc",
            "78451de3c4d27e495f8f3982863911c7455ef9022cce5597b0bd6fc43d7ec83f",
            "99ae03e9056f07bde68b1ec8b1bc7d3e283ec36b3f87e0f791f8499b90163443",
            "bb8ade762653bc566c4f0735f620669f4f3f458d1b29acd7b83289f95fd11c2b",
        ]
    );
    assert_eq!(
        ws.outputs("dbg-cache", &dbg),
        [
            "//g:c\tACTIONS/72598e33a3484a6ece9c52dfe645af70c21c4e82a0daa929e35c1e649fe00dcc/outputs/bazel-out/rust-dbg/bin/g/c.txt",
            "//g:final\tACTIONS/bb8ade762653bc566c4f0735f620669f4f3f458d1b29acd7b83289f95fd11c2b/outputs/bazel-out/rust-dbg/bin/g/final.txt",
        ]
    );
}

#[test]
fn existing_genrule_fixture_keys_are_unchanged() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/basic");
    let files = ["MODULE.bazel", "BUILD.bazel", "input.txt"]
        .map(|name| (name, fs::read_to_string(fixture.join(name)).unwrap()));
    let ws = Workspace::new(
        &files
            .each_ref()
            .map(|(name, content)| (*name, content.as_str())),
    );
    let output = ws.build("cache", &["//:upper", "//:copy"]);
    assert_eq!(
        ws.keys("cache"),
        [
            "827d15fcd4ef1f1e5bf9ab808cab4db4ab2ad43e23a2b1869ec8b9bd032a6948",
            "b24966cb54a67d7474b3d04d6a5e95a13b5d8abe1819771c41726642de2de21e",
        ]
    );
    assert!(summary(&output).contains("2 actions executed, 0 cache hits"));
}
