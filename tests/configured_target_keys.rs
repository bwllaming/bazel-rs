//! N18a (explicit configuration plumbing, docs/CONFIGURATION.md
//! "Configured-target keys and output roots"): keying analysis by
//! configured-target keys and taking output roots from the target's
//! configuration changes no visible behavior. The action cache keys of these
//! builds are golden values recorded with the pre-N18a binary (main 040d055):
//! equal keys mean that warm caches of existing builds stay valid. Action
//! keys frame the host OS and architecture (src/action.rs), so the golden
//! values are those of linux/x86_64.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

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
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={cache}"))
            .arg("build")
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
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
        // The pinned action keys of the fixture's own actions. The two
        // workspace status write actions (bazel-out/{stable,volatile}
        // -status.txt, planned on every executing build since the N63
        // follow-up) are system actions whose keys embed the host name,
        // user and timestamp; they are not the pinned surface and are
        // excluded by their outputs.
        let mut keys = fs::read_dir(self.path().join(cache).join("actions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                !path.join("outputs/bazel-out/stable-status.txt").exists()
                    && !path.join("outputs/bazel-out/volatile-status.txt").exists()
            })
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
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

/// The native `bazel-rs: N actions executed, …` summary line.
fn summary(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .rfind(|line| line.starts_with("bazel-rs: "))
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
// (integration-14; docs/ACTION_ENV.md and docs/REMOTE_CACHE.md): the key
// format is native-action-v8 (manifests native-action-v9 since fix-digest-memo,
// which changes no key); D1 is closed (`k8-*` output roots),
// genrules run Bazel's argv (`source .../genrule-setup.sh; CMD`, the setup
// script a keyed input), run_shell pads `$0` only with arguments, and every
// spawn's environment is N42's (fixed map, inherited names, resolved
// client values), executed exactly as keyed (plus TMPDIR). Re-pinned for
// native-action-v10 (snapshot-keys, SURVEY3 S1): the action id (owner) is no
// longer keyed, the mnemonic is; every key changed.
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
            "3b45fbfbdf9b9f9c136fd74f4d6d245aa3af5b331f0c29e885dca58bd5166c5d",
            "4b6f5f9c319b62409764d640d236c62e655921ffb88579b381677e31d7d617cb",
            "59dfae33e47e08c9b96851b4b5dc2f5a70bdb1b55245af05cbe3329cb91bfcbd",
            "8f3a2f14c8deed65f13de8a89ecde13eb46f3a2a10b01c99c20eb28fd0f6efcc",
            "9ce440264a90b08e52ea2a7c13fa3dbc9550869acc4ffcaadf4c2d96b30571be",
            "ca7c781fac35b77885d738c96d3c5de5f1cc5ba039d678add11cf58494514a54",
        ]
    );
    assert_eq!(
        ws.outputs("default-cache", &cold),
        [
            "//g:c\tACTIONS/9ce440264a90b08e52ea2a7c13fa3dbc9550869acc4ffcaadf4c2d96b30571be/outputs/bazel-out/k8-fastbuild/bin/g/c.txt",
            "//g:final\tACTIONS/3b45fbfbdf9b9f9c136fd74f4d6d245aa3af5b331f0c29e885dca58bd5166c5d/outputs/bazel-out/k8-fastbuild/bin/g/final.txt",
            "//g:u\tACTIONS/ca7c781fac35b77885d738c96d3c5de5f1cc5ba039d678add11cf58494514a54/outputs/bazel-out/k8-fastbuild/bin/g/u.txt",
        ]
    );
    assert!(summary(&cold).contains("6 actions executed, 0 cache hits"));
    let final_txt = fs::read_to_string(
        fs::read_dir(ws.path().join("default-cache/actions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.ends_with("3b45fbfbdf9b9f9c136fd74f4d6d245aa3af5b331f0c29e885dca58bd5166c5d")
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
            "15cc5525bb0a90f0ef77a22f8059a1ca0a54ad6cdc8800b466fc4f45aafe2cd6",
            "420c3a64dc638b1b034870ccee4ea676bdb22eafeb0825e844168b089519600d",
            "a3ff5ceee97bffb65057fbed8b04174e9d283db2546e488c0a4ce03ee093e42c",
            "a5b572d3b796ee870f5c1b78ce3f5124c93d57eb25e031da1920dd45997e4fd4",
            "c33cdef5a03752819dd4b21ffff1b912eb33ad32ab60f0029573f90b2a43da61",
            "f63c0f8a06bc60bda6cc35c578ecb538453851b9759d6db9dcfbfb61b46d593e",
        ]
    );
    assert_eq!(
        ws.outputs("dbg-cache", &dbg),
        [
            "//g:c\tACTIONS/f63c0f8a06bc60bda6cc35c578ecb538453851b9759d6db9dcfbfb61b46d593e/outputs/bazel-out/k8-dbg/bin/g/c.txt",
            "//g:final\tACTIONS/420c3a64dc638b1b034870ccee4ea676bdb22eafeb0825e844168b089519600d/outputs/bazel-out/k8-dbg/bin/g/final.txt",
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
            "07eda685d962aa7e4d3be401db58ab86cb17586713783057f0b655d652c87158",
            "8c1541fa858e39df90af684a0457103c98fd1df859e98baf253b18de290a1cbe",
        ]
    );
    assert!(summary(&output).contains("2 actions executed, 0 cache hits"));
}
