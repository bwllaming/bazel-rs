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

// The pinned native action keys. N41 rekeyed them once (docs/REMOTE_CACHE.md):
// D1 is closed (`k8-*` output roots), genrules run Bazel's argv
// (`source .../genrule-setup.sh; CMD`, PATH env, the setup script input)
// and the native cache format is native-action-v7 (spawns run with exactly
// their keyed environment).
#[test]
fn action_keys_and_output_roots_match_the_pre_n18a_binary() {
    let ws = Workspace::new(FIXTURE);
    let targets = ["--host_platform=//p:linux", "//g:final", "//g:c", "//g:u"];
    let cold = ws.build("default-cache", &targets);
    // Deliberately re-pinned for N42 (docs/ACTION_ENV.md, key format
    // native-action-v6): every key changed. Genrules now run with Bazel's
    // configuration action environment (PATH=/bin:/usr/bin:/usr/local/bin,
    // no LANG/LC_ALL), and the native PATH/LANG/LC_ALL constant is no longer
    // framed into every key (writes included); the environment is framed as
    // fixed map, inherited names and resolved client values. (N23d had
    // re-pinned the three run_shell keys for the exact Starlark environment.)
    assert_eq!(
        ws.keys("default-cache"),
        [
            "11030ee4e782b318d5640ac1fcc9f7b5d8578895582ce216152c59b342dc51c6",
            "3960c70b19b130b0693f17736f303d5ec583a387a19c8ecbd3f64144a0612543",
            "3b01a9dccd47ae94f0a3b4a51470379aea2a3346ac59d7fd4ada7d8b4d6c30ff",
            "6058b04e540df1675767c990713b5507bf005b4b999e8cce492c8127195704f2",
            "8aada8c6738e4ace6d47e999cf824f528ac9a196a638b04e8da48b9759c8cbe8",
            "c60fb2f4e69892a8f78fcb7eaf25d3a6d7e8323d1c8707b64ce36aee86c1b603",
        ]
    );
    assert_eq!(
        ws.outputs("default-cache", &cold),
        [
            "//g:c\tACTIONS/c60fb2f4e69892a8f78fcb7eaf25d3a6d7e8323d1c8707b64ce36aee86c1b603/outputs/bazel-out/k8-fastbuild/bin/g/c.txt",
            "//g:final\tACTIONS/3b01a9dccd47ae94f0a3b4a51470379aea2a3346ac59d7fd4ada7d8b4d6c30ff/outputs/bazel-out/k8-fastbuild/bin/g/final.txt",
            "//g:u\tACTIONS/6058b04e540df1675767c990713b5507bf005b4b999e8cce492c8127195704f2/outputs/bazel-out/k8-fastbuild/bin/g/u.txt",
        ]
    );
    assert!(summary(&cold).contains("6 actions executed, 0 cache hits"));
    let final_txt = fs::read_to_string(
        fs::read_dir(ws.path().join("default-cache/actions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.ends_with("3b01a9dccd47ae94f0a3b4a51470379aea2a3346ac59d7fd4ada7d8b4d6c30ff")
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
            "4aaeadae82d4dab8603e1e7d8422c295e009f88559bae16dedb1a1b462a87acb",
            "5d52495a3673511bf545abbe0c0324253207345d47ac28732f7e6365cf0b68f6",
            "830ca6ab7d268a5bb38c676db5fdc90cbc899d6aee9ee97c15769c4261455b8a",
            "c6d36ff3afbd6b3feb7b8276c8a0d275505a027384d3192b1636b10ede3a509a",
            "ccebf87fccbd15f4612ed44d0df3f11c5f4959acc0640b70fee04bb3ac8ded5e",
            "f9996de0e7750bba7965c4d89d029cb40c487f5133dc78c85b2b85825ec0cd1c",
        ]
    );
    assert_eq!(
        ws.outputs("dbg-cache", &dbg),
        [
            "//g:c\tACTIONS/4aaeadae82d4dab8603e1e7d8422c295e009f88559bae16dedb1a1b462a87acb/outputs/bazel-out/k8-dbg/bin/g/c.txt",
            "//g:final\tACTIONS/ccebf87fccbd15f4612ed44d0df3f11c5f4959acc0640b70fee04bb3ac8ded5e/outputs/bazel-out/k8-dbg/bin/g/final.txt",
        ]
    );
}

/// Re-pinned for N42 (docs/ACTION_ENV.md): genrules run with the strict
/// configuration environment and the key format is native-action-v6; the
/// keys stay stable across builds.
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
            "070b0f6973d83f4dd2c4d4f446008d1730c9b9def52f64a895612d7784c85812",
            "6eac64d5b44bf36fe44662922d5e270dfa8d8abfb97f8faf09d7671495a6d67e",
        ]
    );
    assert!(summary(&output).contains("2 actions executed, 0 cache hits"));
}
