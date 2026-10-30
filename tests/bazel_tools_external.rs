//! `external/bazel_tools` beside every repository's directory, like genuine
//! Bazel 9.3.1's `<output_base>/external/bazel_tools` (a link to the install
//! base's `embedded_tools`): ~/code's `--repo_env=JAVA_HOME=../bazel_tools/jdk`
//! names the embedded JDK from any repository rule (rules_jvm_external's
//! coursier: `rctx.path(JAVA_HOME + "/bin/java")`; rules_java's
//! local_java_repository links the JDK's entries). Natively the slot
//! `<cache>/repositories/external/bazel_tools` links the embedded tools tree
//! (docs/REPOSITORY_SLOTS.md, "The bazel_tools slot"). Expected results:
//! tests/fixtures/bazel_tools_external/reference_receipt.json (capture.py
//! there). Uses the local builtin-module fixture registry only; never a
//! reference Bazel.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
include!("support/temp_root.rs");
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bazel_tools_external"
);
const REGISTRY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/builtin_modules/registry"
);

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn receipt() -> serde_json::Value {
    serde_json::from_slice(&fs::read(Path::new(FIXTURE).join("reference_receipt.json")).unwrap())
        .unwrap()
}

/// Genuine's capture command, natively (the default built-in profile).
fn query(root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", root.join("cache").display()))
        .args(["query", "@probe//:all + @jdk//:all"])
        .arg("--repo_env=JAVA_HOME=../bazel_tools/jdk")
        .arg(format!("--registry=file://{REGISTRY}"))
        .arg("--repository_disable_download")
        .current_dir(root.join("ws"))
        .env_clear()
        .envs(verifier_environment())
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.join("home"))
        .env("TMPDIR", root.join("tmp"))
        .env("LC_ALL", "C.UTF-8")
        .output()
        .unwrap()
}

#[test]
fn repository_rules_find_the_embedded_jdk_at_external_bazel_tools_like_genuine() {
    let receipt = receipt();
    assert_eq!(receipt["binary"], "bazel-9.3.1-linux-x86_64");
    let dir = tempfile::Builder::new()
        .prefix("bazel-tools-external-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    copy_tree(
        &Path::new(FIXTURE).join("workspace"),
        &dir.path().join("ws"),
    );
    for name in ["home", "tmp"] {
        fs::create_dir(dir.path().join(name)).unwrap();
    }
    let external = dir.path().join("cache/repositories/external");
    let check = |output: Output| {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            receipt["stdout"].as_str().unwrap()
        );
        for (name, canonical) in [("probe", "+probe+probe"), ("jdk", "+jdk_like+jdk")] {
            assert_eq!(
                fs::read_to_string(external.join(canonical).join("result.txt")).unwrap(),
                receipt["results"][name].as_str().unwrap(),
                "{name}"
            );
        }
        // The slot links the embedded tools tree; its JDK keeps its modes.
        let link = fs::read_link(external.join("bazel_tools")).unwrap();
        assert!(link.starts_with("../builtins/trees"), "{}", link.display());
        assert_eq!(receipt["layout"]["bazel_tools_is_symlink"], true);
        let java = fs::metadata(external.join("bazel_tools/jdk/bin/java")).unwrap();
        assert_eq!(
            format!("{:#o}", java.permissions().mode() & 0o7777),
            receipt["layout"]["java_mode"]
        );
        // rules_java's links into it (genuine: `<EXTERNAL>/bazel_tools/jdk/bin`)
        // are pinned to the tree, never through the slot.
        let bin = fs::read_link(external.join("+jdk_like+jdk/bin")).unwrap();
        assert!(
            !bin.starts_with(&external) && bin.ends_with("jdk/bin"),
            "{}",
            bin.display()
        );
        assert_eq!(
            fs::canonicalize(&bin).unwrap(),
            fs::canonicalize(external.join("bazel_tools/jdk/bin")).unwrap()
        );
    };
    check(query(dir.path()));
    // A slot missing (an earlier binary's cache, a collection) is bound
    // again when a repository rule runs; a modified one is rebound too.
    fs::remove_file(external.join("bazel_tools")).unwrap();
    fs::remove_file(dir.path().join("ws/MODULE.bazel.lock")).ok();
    fs::write(
        dir.path().join("ws/probe.bzl"),
        fs::read_to_string(dir.path().join("ws/probe.bzl")).unwrap() + "\n# edited\n",
    )
    .unwrap();
    check(query(dir.path()));
}
