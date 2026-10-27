//! integration-15: N50's rule `attr.label_keyed_string_dict` values with N45's
//! deferred unresolvable repositories (also inside select() branches) and
//! N46's implicit-outputs template check.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", root.join("cache").display()))
        .args(args)
        .current_dir(root.join("ws"))
        .env_clear()
        .envs(verifier_environment())
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .explicit_modules()
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn label_keyed_dict_keys_defer_unknown_repositories_and_reject_output_templates() {
    let dir = tempfile::Builder::new()
        .prefix("lksd-merge-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    let root = dir.path();
    write(root, "ws/MODULE.bazel", "module(name = \"lk\")\n");
    write(root, "ws/a.txt", "a\n");
    write(
        root,
        "ws/defs.bzl",
        r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, str(sorted([str(t.label) + "=" + v for t, v in ctx.attr.d.items()])))
    return [DefaultInfo(files = depset([out]))]
r = rule(_impl, attrs = {"d": attr.label_keyed_string_dict(allow_files = True)})
o = rule(_impl, attrs = {"d": attr.label_keyed_string_dict(allow_files = True)}, outputs = {"x": "%{d}.out"})
"#,
    );
    write(
        root,
        "ws/BUILD.bazel",
        "load(\":defs.bzl\", \"r\")\nr(name = \"ok\", d = {\"a.txt\": \"a\"})\nr(name = \"plain\", d = {\"@nope//:x\": \"v\"})\nr(name = \"sel\", d = select({\"//conditions:default\": {\"@nope//:y\": \"w\"}}))\n",
    );
    write(
        root,
        "ws/io/BUILD.bazel",
        "load(\"//:defs.bzl\", \"o\")\no(name = \"x\", d = {\"//:a.txt\": \"a\"})\n",
    );
    // Loading keeps the unknown repository as Bazel's deferred invalid label
    // (N45), in plain values and select() branches alike.
    let query = run(root, &["query", "//:all"]);
    assert!(query.status.success(), "{}", stderr(&query));
    assert_eq!(
        String::from_utf8_lossy(&query.stdout),
        "//:ok\n//:plain\n//:sel\n"
    );
    let ok = run(root, &["build", "//:ok"]);
    assert!(ok.status.success(), "{}", stderr(&ok));
    for target in ["//:plain", "//:sel"] {
        let failed = run(root, &["build", target]);
        assert!(!failed.status.success());
        assert!(
            stderr(&failed).contains("No repository visible as '@nope' from main repository"),
            "{}",
            stderr(&failed)
        );
    }
    // ImplicitOutputsFunction: dict(label, string) is no template type (N46).
    let io = run(root, &["build", "//io:x"]);
    assert!(!io.status.success());
    assert!(
        stderr(&io).contains("For attribute 'd' in outputs: Attributes of type dict(label, string) cannot be used in an outputs substitution template"),
        "{}",
        stderr(&io)
    );
}
