//! TD companion contracts: actual mutable-state isolation and genuine root mapping.
#![allow(clippy::disallowed_methods)] // isolated subprocess harness

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
    home: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("td-support-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let top = temp.path();
        let root = top.join("ws");
        let home = top.join("home");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(root.join("MODULE.bazel"), "module(name = 'scratch')\n").unwrap();
        fs::write(
            root.join("BUILD.bazel"),
            "genrule(name='g', srcs=['in.txt'], outs=['out.txt'], cmd='cat $(SRCS) > $@')\n",
        )
        .unwrap();
        fs::write(root.join("in.txt"), "one\n").unwrap();
        Self {
            root,
            home,
            cache: top.join("shared"),
            _temp: temp,
        }
    }

    fn run(&self, startup: &[&str], args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .args(["--nosystem_rc", "--nohome_rc"])
            .arg(format!("--cache-dir={}", self.cache.display()))
            .args(startup)
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .output()
            .unwrap()
    }

    fn ok(&self, startup: &[&str], args: &[&str]) -> String {
        let out = self.run(startup, args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

#[test]
fn explicit_output_bases_isolate_state_but_share_action_content() {
    let ws = Workspace::new();
    // Reproducible extension results go into the hidden lockfile, not the
    // workspace lockfile. Demand one so this tests the actual write path.
    fs::write(ws.root.join("MODULE.bazel"), "module(name='scratch')\next = use_extension('//:ext.bzl', 'ext')\nuse_repo(ext, 'generated')\n").unwrap();
    fs::write(ws.root.join("ext.bzl"), "def _repo(ctx):\n    ctx.file('BUILD.bazel', 'exports_files([\"data\"])')\n    ctx.file('data', '')\nr = repository_rule(implementation=_repo)\ndef _ext(ctx):\n    r(name='generated')\n    return ctx.extension_metadata(reproducible=True)\next = module_extension(implementation=_ext)\n").unwrap();
    fs::write(ws.root.join("BUILD.bazel"), "genrule(name='g', srcs=['in.txt', '@generated//:data'], outs=['out.txt'], cmd='cat $(SRCS) > $@')\n").unwrap();
    let a = ws.home.join("a");
    let b = ws.home.join("b");
    let a_flag = format!("--output_base={}", a.display());
    let b_flag = format!("--output_base={}", b.display());
    let original = ws.ok(&[], &["info", "output_base"]);
    assert_eq!(
        ws.ok(&[&a_flag], &["info", "output_base"]),
        format!("{}\n", a.display())
    );
    assert!(!a.exists(), "info must not create an output base");
    ws.ok(&[&a_flag], &["build", "//:g"]);
    let a_bin = PathBuf::from(ws.ok(&[&a_flag], &["info", "bazel-bin"]).trim());
    assert!(a_bin.starts_with(&a));
    assert_eq!(fs::read_to_string(a_bin.join("out.txt")).unwrap(), "one\n");
    assert!(a.join("snapshots").is_dir());
    assert!(a.join("MODULE.bazel.lock").is_file());
    let original_lock = fs::read(a.join("MODULE.bazel.lock")).unwrap();
    fs::write(ws.root.join("in.txt"), "two\n").unwrap();
    ws.ok(&[&b_flag], &["build", "//:g"]);
    let b_bin = PathBuf::from(ws.ok(&[&b_flag], &["info", "bazel-bin"]).trim());
    assert!(b_bin.starts_with(&b));
    assert_eq!(fs::read_to_string(b_bin.join("out.txt")).unwrap(), "two\n");
    assert_eq!(fs::read_to_string(a_bin.join("out.txt")).unwrap(), "one\n");
    assert_eq!(
        fs::read(a.join("MODULE.bazel.lock")).unwrap(),
        original_lock
    );
    assert!(b.join("snapshots").is_dir());
    assert!(!Path::new(original.trim()).exists());
    assert!(ws.cache.join("actions").is_dir());
    fs::write(ws.root.join("in.txt"), "one\n").unwrap();
    let out = ws.run(&["--output_base", a.to_str().unwrap()], &["build", "//:g"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("0 actions executed"));
    assert_eq!(fs::read_to_string(b_bin.join("out.txt")).unwrap(), "two\n");
    // TD forwards the inferred default output base verbatim; it must not be
    // rehashed into a different directory or change the shared cache.
    assert_eq!(
        ws.ok(
            &["--output_base", original.trim()],
            &["info", "output_base"]
        ),
        original
    );
    assert_eq!(
        ws.ok(&[&a_flag], &["info", "repository_cache"]),
        ws.ok(&[&b_flag], &["info", "repository_cache"])
    );
    ws.ok(&[&a_flag], &["clean", "--expunge"]);
    assert!(
        !a.exists(),
        "clean must remove the explicitly selected base"
    );
    assert_eq!(fs::read_to_string(b_bin.join("out.txt")).unwrap(), "two\n");
    assert!(ws.cache.join("actions").is_dir());
    assert!(!Path::new(original.trim()).exists());
}

#[test]
fn startup_output_base_rc_precedence_relative_paths_and_symlinks() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join(".bazelrc"),
        "startup --output_base first\nstartup:linux --output_base=second\n",
    )
    .unwrap();
    assert_eq!(
        ws.ok(&[], &["info", "output_base"]),
        format!("{}\n", ws.root.join("second").display())
    );
    assert_eq!(
        ws.ok(
            &["--output_base=third", "--output_base", "fourth/../final"],
            &["info", "output_base"]
        ),
        format!("{}\n", ws.root.join("final").display())
    );
    std::os::unix::fs::symlink(&ws.home, ws.root.join("link")).unwrap();
    assert_eq!(
        ws.ok(&["--output_base=link/child"], &["info", "output_base"]),
        format!("{}\n", ws.home.join("child").display())
    );
    for flag in ["--output_base=", "--output_base=in.txt"] {
        assert!(!ws.run(&[flag], &["info", "output_base"]).status.success());
    }
}

#[test]
fn output_base_and_quiet_startup_options_both_take_effect() {
    let ws = Workspace::new();
    for (rc, startup, directory) in [
        (
            "startup --quiet\n",
            vec!["--output_base=from_cli"],
            "from_cli",
        ),
        (
            "startup --output_base=from_rc\n",
            vec!["--quiet"],
            "from_rc",
        ),
    ] {
        fs::write(ws.root.join(".bazelrc"), rc).unwrap();
        let out = ws.run(&startup, &["info", "output_base"]);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("{}\n", ws.root.join(directory).display())
        );
        assert!(out.stderr.is_empty(), "{:?}", out.stderr);
        assert!(!ws.root.join(directory).exists());
        let built = ws.run(&startup, &["build", "//:g"]);
        assert!(built.status.success(), "{built:?}");
        assert!(built.stderr.is_empty(), "{:?}", built.stderr);
        assert!(ws.root.join(directory).join("execroot").is_dir());
    }
}

#[test]
fn dump_root_mapping_uses_real_module_aliases_and_extension_imports() {
    let ws = Workspace::new();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in [
        "MODULE.bazel",
        "BUILD.bazel",
        "repos.bzl",
        "dep/MODULE.bazel",
    ] {
        let path = ws.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(fixtures.join("td_support").join(name), path).unwrap();
    }
    let registry = format!(
        "--registry=file://{}",
        fixtures.join("builtin_modules/registry").display()
    );
    let text = ws.ok(
        &[],
        &[
            "mod",
            "dump_repo_mapping",
            "",
            "--lockfile_mode=off",
            &registry,
        ],
    );
    // Captured with genuine 9.3.0, same MODULE and .bzl fixture; compact JSON,
    // canonical root is empty and extension repository is not materialized.
    assert_eq!(
        text,
        "{\"generated\":\"+ext+generated\",\"\":\"\",\"root_alias\":\"\",\"renamed\":\"dep+\",\"bazel_tools\":\"bazel_tools\"}\n"
    );
    let output = ws.run(
        &[],
        &[
            "mod",
            "dump_repo_mapping",
            "missing",
            "--lockfile_mode=off",
            &registry,
        ],
    );
    assert!(!output.status.success());
}

#[test]
fn td_release_and_semantics_are_machine_readable_without_hiding_native_identity() {
    let ws = Workspace::new();
    assert_eq!(
        ws.ok(&[], &["info", "release"]),
        format!("release 9.3.0+bazel-rs.{}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        ws.ok(&[], &["info", "starlark-semantics"]),
        "StarlarkSemantics{internal_bazel_only_utf_8_byte_strings=true}\n"
    );
}
