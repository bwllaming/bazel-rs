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

// The pinned native action keys, re-pinned once for the N41 x N42 merge
// (integration-14; docs/ACTION_ENV.md and docs/REMOTE_CACHE.md): the key and
// manifest format is native-action-v8; D1 is closed (`k8-*` output roots),
// genrules run Bazel's argv (`source .../genrule-setup.sh; CMD`, the setup
// script a keyed input), run_shell pads `$0` only with arguments, and every
// spawn's environment is N42's (fixed map, inherited names, resolved
// client values), executed exactly as keyed (plus TMPDIR).
#[test]
fn action_keys_and_output_roots_match_the_pre_n18a_binary() {
    let ws = Workspace::new(FIXTURE);
    let targets = ["--host_platform=//p:linux", "//g:final", "//g:c", "//g:u"];
    let cold = ws.build("default-cache", &targets);
    // Deliberately re-pinned (see above; before: N42's native-action-v6,
    // which itself re-pinned every key): every key changed. Genrules now run with Bazel's
    // configuration action environment (PATH=/bin:/usr/bin:/usr/local/bin,
    // no LANG/LC_ALL), and the native PATH/LANG/LC_ALL constant is no longer
    // framed into every key (writes included); the environment is framed as
    // fixed map, inherited names and resolved client values. (N23d had
    // re-pinned the three run_shell keys for the exact Starlark environment.)
    assert_eq!(
        ws.keys("default-cache"),
        [
            "081333d3a04df20bd5e97d2ad171bcbfdedda07b2ca6cf58c0e941e3f3b674b6",
            "80276d639eafdc42187255bf2817ac38ed6be835a96e5677d515efbcda79945a",
            "a59b787dd6adf44d47f4256e85703f469a30382f36768f044ec3f318b3610add",
            "ac86cfdc1c87e32b3be3a0e6ca0fe125e294fc58a13988ac2125f831a91f0d0c",
            "c0487c445714832edaa008badf63f01313458beb0bfc32a2e3c8f63ed0b03cc1",
            "cde8b6368c40107ba1efba8c2fe924f9e54c1248d6fc26cdb523a787b55246cc",
        ]
    );
    assert_eq!(
        ws.outputs("default-cache", &cold),
        [
            "//g:c\tACTIONS/cde8b6368c40107ba1efba8c2fe924f9e54c1248d6fc26cdb523a787b55246cc/outputs/bazel-out/k8-fastbuild/bin/g/c.txt",
            "//g:final\tACTIONS/80276d639eafdc42187255bf2817ac38ed6be835a96e5677d515efbcda79945a/outputs/bazel-out/k8-fastbuild/bin/g/final.txt",
            "//g:u\tACTIONS/c0487c445714832edaa008badf63f01313458beb0bfc32a2e3c8f63ed0b03cc1/outputs/bazel-out/k8-fastbuild/bin/g/u.txt",
        ]
    );
    assert!(summary(&cold).contains("6 actions executed, 0 cache hits"));
    let final_txt = fs::read_to_string(
        fs::read_dir(ws.path().join("default-cache/actions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.ends_with("80276d639eafdc42187255bf2817ac38ed6be835a96e5677d515efbcda79945a")
            })
            .unwrap()
            .join("outputs/bazel-out/k8-fastbuild/bin/g/final.txt"),
    )
    .unwrap();
    assert!(final_txt.starts_with(r#"{"a":{"path":"bazel-out/k8-fastbuild/bin/g/gen.a.txt","root":"bazel-out/k8-fastbuild/bin","short_path":"g/gen.a.txt"}"#), "{final_txt}");
    assert!(final_txt.ends_with("bazel-out/k8-fastbuild/bin/g/b.txt bazel-out/k8-fastbuild/bin/g/gen.a.txt bazel-out/k8-fastbuild/bin/g\n"), "{final_txt}");
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
            "2261a84d8c11a08dffce7e217eb73c99ef89d93866e09d6385abce3460379ddc",
            "34e68fe10d1e6cd03bcb66428a936fe54a614605433b91336d8c75dbcdef0bb8",
            "6995ea95bd12d702570e9ec056067d52cccde454d1c35e4a0414afedb7cff48a",
            "9689167d0d64a7c138e94c00b099c9fed9d4088ed2ae183f93b7f6edc2524776",
            "c3348a31c0d656311deee3a3e9ae8c7d73ebf350ac2024189f37fb45c9dc5bb3",
            "e54b1319ee442d963c8c6c307f44552e0f4cc308e4bc00a3c120e97cef439357",
        ]
    );
    assert_eq!(
        ws.outputs("dbg-cache", &dbg),
        [
            "//g:c\tACTIONS/c3348a31c0d656311deee3a3e9ae8c7d73ebf350ac2024189f37fb45c9dc5bb3/outputs/bazel-out/k8-dbg/bin/g/c.txt",
            "//g:final\tACTIONS/6995ea95bd12d702570e9ec056067d52cccde454d1c35e4a0414afedb7cff48a/outputs/bazel-out/k8-dbg/bin/g/final.txt",
        ]
    );
}

/// Re-pinned for the N41 x N42 merge (native-action-v8; genrules run Bazel's
/// argv with genrule-setup.sh in their configuration's environment, k8
/// roots; before: N42's v6); the keys stay stable across builds. N53
/// re-pinned `//:copy`: `$(location :inputs)` of a root-package file is
/// PathFragment.getCallablePathString's `./input.txt` (LocationExpander.java:327-336).
#[test]
fn existing_genrule_fixture_keys_are_pinned() {
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
            "0966e42b771a732a54937cf7e805869a4a77f70ada64f475a97c7a633c43a6e6",
            "1a94ed1182cac118df6e7c6c3c0bf201eb1fef61dc8cff613d512760a5070e30",
        ]
    );
    assert!(summary(&output).contains("2 actions executed, 0 cache hits"));
}
