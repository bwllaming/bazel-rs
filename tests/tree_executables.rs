//! A generated File executable that is an `actions.symlink` alias of a file
//! of an external repository runs through a symlink into that repository's
//! published tree (docs/HARDLINK_STAGING.md, "Executables aliasing
//! repository files"): a program that locates its installation from its own
//! resolved path — CPython's prefix search from `realpath(argv[0])`, here
//! `readlink -f "$0"` — finds the repository beside it, as it does through
//! Bazel's symlinked sandbox and execroot, under both strategies.
//!
//! rules_python 1.7's PyCompile runs
//! `bazel-out/<exec>/bin/external/rules_python+/python/private/python3`, an
//! `actions.symlink` of the hermetic interpreter
//! (py_exec_tools_toolchain.bzl `current_interpreter_executable`). Staged as
//! a hardlinked blob, it resolved to itself and failed with `Could not find
//! platform independent libraries <prefix>` / `No module named 'encodings'`.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const MODULE: &str = r##"module(name = "tree_executables")

tool_repo = use_repo_rule("//:repo.bzl", "tool_repo")

tool_repo(name = "tools")
"##;

/// An installation that finds its data relative to its resolved location,
/// like a hermetic interpreter's `bin/python3` and `lib/python3.X/`.
const REPO: &str = r##"def _impl(ctx):
    ctx.file(
        "bin/tool",
        "#!/bin/sh\nhome=$(dirname \"$(dirname \"$(readlink -f \"$0\")\")\")\ncat \"$home/lib/landmark\" > \"$1\"\n",
        executable = True,
    )
    ctx.file("lib/landmark", "found beside the resolved executable\n")
    ctx.file("BUILD.bazel", 'exports_files(["bin/tool", "lib/landmark"])\n')

tool_repo = repository_rule(_impl)
"##;

const DEFS: &str = r##"def _aliased_impl(ctx):
    # rules_python's current_interpreter_executable: an executable named
    # after the interpreter, symlinked to it; its runfiles carry the
    # installation, which the program never looks at.
    executable = ctx.actions.declare_file(ctx.label.name + "/tool")
    ctx.actions.symlink(output = executable, target_file = ctx.file.tool, is_executable = True)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run(
        executable = executable,
        arguments = [out.path],
        outputs = [out],
        tools = ctx.files.data,
        mnemonic = "RunTool",
    )
    return [DefaultInfo(files = depset([out]))]

aliased = rule(
    _aliased_impl,
    attrs = {
        "tool": attr.label(allow_single_file = True, cfg = "exec"),
        "data": attr.label_list(allow_files = True, cfg = "exec"),
    },
)

def _direct_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run(
        executable = ctx.file.tool,
        arguments = [out.path],
        outputs = [out],
        tools = ctx.files.data,
        mnemonic = "RunTool",
    )
    return [DefaultInfo(files = depset([out]))]

direct = rule(
    _direct_impl,
    attrs = {
        "tool": attr.label(allow_single_file = True, cfg = "exec"),
        "data": attr.label_list(allow_files = True, cfg = "exec"),
    },
)
"##;

const BUILD: &str = r##"load(":defs.bzl", "aliased", "direct")

aliased(name = "aliased", tool = "@tools//:bin/tool")

# At its own execution path the executable keeps hardlink staging: its
# declared installation mirrors the repository around it.
direct(name = "direct", tool = "@tools//:bin/tool", data = ["@tools//:lib/landmark"])
"##;

struct Workspace(tempfile::TempDir);

impl Drop for Workspace {
    fn drop(&mut self) {
        // Published outputs and repository trees may be read-only.
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.path())
            .status();
    }
}

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("tree-executables-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        for (name, text) in [
            ("MODULE.bazel", MODULE),
            ("repo.bzl", REPO),
            ("defs.bzl", DEFS),
            ("BUILD.bazel", BUILD),
        ] {
            fs::write(ws.path().join(name), text).unwrap();
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn run(&self, cache: &str, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", &format!("--cache-dir={cache}")])
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

#[test]
fn aliased_repository_executables_resolve_into_their_repository() {
    let ws = Workspace::new();
    for (cache, strategy) in [("sandboxed", None), ("standalone", Some("standalone"))] {
        let mut args = vec![
            "build".to_owned(),
            "--experimental-builtin-modules".to_owned(),
        ];
        if let Some(strategy) = strategy {
            args.push(format!("--spawn_strategy={strategy}"));
        }
        args.extend(["//:aliased".to_owned(), "//:direct".to_owned()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = ws.run(cache, &args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{cache}: {stderr}");
        let mode = if strategy.is_some() {
            "not sandboxed"
        } else {
            "sandboxed"
        };
        assert!(
            stderr.contains(&format!("(experimental local execution; {mode})")),
            "{cache}: {stderr}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        for name in ["aliased", "direct"] {
            let path = stdout
                .lines()
                .filter_map(|line| line.split_once('\t'))
                .map(|(_, path)| path)
                .find(|path| path.ends_with(&format!("/{name}.txt")))
                .unwrap_or_else(|| panic!("{cache}: no {name}.txt in {stdout}"));
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                "found beside the resolved executable\n",
                "{cache} {name}"
            );
        }
    }
}
