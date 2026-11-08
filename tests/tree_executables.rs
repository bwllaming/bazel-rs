//! A generated File executable that is an `actions.symlink` alias of a file
//! of an external repository runs through a symlink into that repository's
//! published tree (docs/HARDLINK_STAGING.md, "Repository files: links into
//! their tree"): a program that locates its installation from its own
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
//!
//! Every other file of an external repository a process sees — an input at
//! its own path, a tool's runfile, a test's runfile — is such a link as well
//! (`runfiles_and_inputs_of_repository_files_link_into_their_repository`,
//! pinned by a genuine capture): rules_nodejs's `bin/node` wrapper follows
//! its own links to run the undeclared `nodejs/bin/node` beside it.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
include!("support/temp_root.rs");
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

include!("support/executable_script.rs");
include!("support/module_archives.rs");

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

# At its own execution path the executable is a link into the repository
# too, like any file of the repository a process sees.
direct(name = "direct", tool = "@tools//:bin/tool", data = ["@tools//:lib/landmark"])

# Only an alias of a repository file runs through a link: aliases of a
# workspace source and of a generated file keep hardlink staging.
aliased(name = "aliased_source", tool = "probe.sh")

genrule(
    name = "generated_probe",
    srcs = ["probe.sh"],
    outs = ["generated_probe.sh"],
    cmd = "cp $(location probe.sh) $@",
    executable = True,
)

aliased(name = "aliased_output", tool = ":generated_probe")
"##;

/// Reports whether the program runs as a link or as a regular file.
const PROBE: &str = "#!/bin/sh\nif [ -L \"$0\" ]; then echo link; else echo file; fi > \"$1\"\n";

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
                .tempdir_in(crate::test_temp::root())
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
        // Not fs::write for an executable: see write_executable_script
        // (ETXTBSY).
        write_executable_script(&ws.path().join("probe.sh"), PROBE, 0o755);
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
            .seeded_archives()
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
        // The built-in modules (the default) from the offline registry.
        let mut args = vec!["build".to_owned(), builtin_registry_flag()];
        if let Some(strategy) = strategy {
            args.push(format!("--spawn_strategy={strategy}"));
        }
        args.extend(
            [
                "//:aliased",
                "//:direct",
                "//:aliased_source",
                "//:aliased_output",
            ]
            .map(str::to_owned),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        // The default host platform materializes @platforms: offline.
        seed_module_archives(&ws.path().join(cache), &["platforms@1.0.0"]);
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
        for (name, expected) in [
            ("aliased", "found beside the resolved executable\n"),
            ("direct", "found beside the resolved executable\n"),
            ("aliased_source", "file\n"),
            ("aliased_output", "file\n"),
        ] {
            let path = stdout
                .lines()
                .filter_map(|line| line.split_once('\t'))
                .map(|(_, path)| path)
                .find(|path| path.ends_with(&format!("/{name}.txt")))
                .unwrap_or_else(|| panic!("{cache}: no {name}.txt in {stdout}"));
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                expected,
                "{cache} {name}"
            );
        }
    }
}

/// How a spawn sees a file of an external repository, pinned by a genuine
/// capture (tests/fixtures/runfiles_repo_links: capture.py,
/// reference_receipt.json): a test's runfile, an input run at its execution
/// path and a tool's runfile in an action, each rules_nodejs's
/// readlink-resolving `bin/node` wrapper that runs its undeclared sibling
/// `bin/real/tool`, standalone and sandboxed. The runfiles are symlinks and
/// every wrapper finds its sibling in the repository tree (bazel-rs staged
/// hardlinked blobs: exit 127, `.../bin/real/tool: No such file or
/// directory`). Genuine's `sandboxed` ran as processwrapper-sandbox on this
/// host.
#[test]
fn runfiles_and_inputs_of_repository_files_link_into_their_repository() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runfiles_repo_links");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture.join("reference_receipt.json")).unwrap())
            .unwrap();
    let ws = Workspace(
        tempfile::Builder::new()
            .prefix("runfiles-repo-links-cli-")
            .tempdir_in(crate::test_temp::root())
            .unwrap(),
    );
    for name in ["MODULE.bazel", "BUILD.bazel", "defs.bzl", "repo.bzl"] {
        fs::copy(fixture.join("workspace").join(name), ws.path().join(name)).unwrap();
    }
    write_executable_script(
        &ws.path().join("tool.sh"),
        fs::read_to_string(fixture.join("workspace/tool.sh")).unwrap(),
        0o755,
    );
    let mut rows = Vec::new();
    for strategy in ["standalone", "sandboxed"] {
        seed_module_archives(&ws.path().join(strategy), &["platforms@1.0.0"]);
        let args = [
            "test".to_owned(),
            builtin_registry_flag(),
            format!("--spawn_strategy={strategy}"),
            "--test_output=all".to_owned(),
            "//:all".to_owned(),
        ];
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = ws.run(strategy, &args);
        let mut text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{strategy}: {text}");
        for name in ["input.txt", "tool_runfiles.txt"] {
            text.push_str(&find_output(
                &ws.path().join(strategy).join("actions"),
                name,
            ));
        }
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| {
                ["test_runfile_", "input_", "tool_runfile_"]
                    .iter()
                    .any(|key| line.starts_with(key))
            })
            .collect();
        rows.push(serde_json::json!({"strategy": strategy, "lines": lines}));
    }
    assert_eq!(serde_json::Value::Array(rows), receipt["rows"]);
}

/// The content of the published output file `name` below `directory`.
fn find_output(directory: &Path, name: &str) -> String {
    let mut pending: Vec<PathBuf> = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if entry.file_name() == name {
                return fs::read_to_string(path).unwrap();
            }
        }
    }
    panic!(
        "no published output named {name} under {}",
        directory.display()
    )
}
