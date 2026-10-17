//! N49b: the single-transitive depset identity shortcut
//! (NestedSetBuilder.buildInternal, Depset.fromDirectAndTransitive; see
//! docs/DEPSETS.md "Identity of a depset built from one transitive set")
//! changes which node a depset value is, never its elements or their order.
//! The action cache keys of this build are golden values recorded with the
//! pre-N49b binary (26c6ba7): run_shell inputs and Args built from every
//! shortcut shape (one transitive set, an empty sibling, a repeated set, a
//! direct element equal to a singleton set, topological, a compatible but
//! different order) and a write of their `to_list()`/repr. Action keys frame
//! the host OS and architecture (src/action.rs), so the golden values are
//! those of linux/x86_64.
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
                .prefix("n49b-")
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

    fn keys(&self, cache: &str) -> Vec<String> {
        let mut keys = fs::read_dir(self.path().join(cache).join("actions"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }
}

const FIXTURE: &[(&str, &str)] = &[
    ("MODULE.bazel", "module(name = \"main\")\n"),
    (
        "k/defs.bzl",
        r#"def _shapes():
    x = depset(["p", "q"])
    t = depset(["p", "q"], order = "topological")
    one = depset(["p"])
    return [
        depset(transitive = [x]),
        depset(transitive = [depset(), x]),
        depset(transitive = [x, depset(order = "topological")]),
        depset(transitive = [x, x]),
        depset(["p"], transitive = [one]),
        depset(["q"], transitive = [one]),
        depset(transitive = [t], order = "topological"),
        depset(transitive = [t, t], order = "topological"),
        depset(transitive = [x], order = "preorder"),
        depset(["r"], transitive = [x, t]),
        depset(transitive = [depset(transitive = [x])]),
    ]

def _leaf_impl(ctx):
    a = ctx.actions.declare_file(ctx.label.name + ".a")
    ctx.actions.write(a, ctx.label.name)
    return [DefaultInfo(files = depset([a]))]

leaf = rule(_leaf_impl)

def _use_impl(ctx):
    shapes = _shapes()
    args = ctx.actions.args()
    for s in shapes:
        args.add_all(s)
        args.add_joined(s, join_with = ",")
    dep = ctx.attr.dep[DefaultInfo].files
    files = depset(transitive = [dep])
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = depset(transitive = [depset(transitive = [files]), depset()]),
        arguments = [args],
        command = "echo \"$@\" > " + out.path,
    )
    w = ctx.actions.declare_file(ctx.label.name + ".w")
    ctx.actions.write(w, repr([s.to_list() for s in shapes]) + "\n" + repr(shapes) + "\n" + repr(files) + "\n")
    return [DefaultInfo(files = depset(transitive = [depset([out, w]), files]))]

use = rule(_use_impl, attrs = {"dep": attr.label()})
"#,
    ),
    (
        "k/BUILD.bazel",
        "load(\":defs.bzl\", \"leaf\", \"use\")\nleaf(name = \"l\")\nuse(name = \"u\", dep = \":l\")\n",
    ),
];

#[test]
fn shortcut_shapes_keep_their_action_keys() {
    let ws = Workspace::new(FIXTURE);
    let output = ws.build("cache", &["//k:u"]);
    assert_eq!(
        ws.keys("cache"),
        [
            "3a80bbfdb3381477ce5493be608aaa89da6fd3bbd7c5a56c9e7f5802b4fa03e0",
            "b5ba4d5eecc2cb75d933ed0074d748f1e229d893145e8e5648a9eb3094e48541",
            "ff200d1e0a17439dd7db3bb1f3dfbaf20a1efa851c1a266bacbd4678669c35c7",
        ]
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let outputs = stdout
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(_, path)| path))
        .map(|path| {
            (
                path.rsplit('/').next().unwrap().to_owned(),
                fs::read_to_string(path).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outputs,
        [
            (
                "u.out".to_owned(),
                "p q p,q p q p,q p q p,q p q p,q p p p q p,q p q p,q p q p,q p q p,q p q r p,q,r p q p,q\n".to_owned()
            ),
            (
                "u.w".to_owned(),
                concat!(
                    "[[\"p\", \"q\"], [\"p\", \"q\"], [\"p\", \"q\"], [\"p\", \"q\"], [\"p\"], [\"p\", \"q\"], [\"p\", \"q\"], [\"p\", \"q\"], [\"p\", \"q\"], [\"p\", \"q\", \"r\"], [\"p\", \"q\"]]\n",
                    "[depset([\"p\", \"q\"]), depset([\"p\", \"q\"]), depset([\"p\", \"q\"]), depset([\"p\", \"q\"]), depset([\"p\"]), depset([\"p\", \"q\"]), depset([\"p\", \"q\"], order = \"topological\"), depset([\"p\", \"q\"], order = \"topological\"), depset([\"p\", \"q\"], order = \"preorder\"), depset([\"p\", \"q\", \"r\"]), depset([\"p\", \"q\"])]\n",
                    "depset([<generated file k/l.a>])\n"
                )
                .to_owned()
            ),
            ("l.a".to_owned(), "l".to_owned()),
        ]
    );
    // A warm build hits every key.
    let warm = ws.build("cache", &["//k:u"]);
    assert!(
        String::from_utf8_lossy(&warm.stderr).contains("0 actions executed, 3 cache hits"),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
}
