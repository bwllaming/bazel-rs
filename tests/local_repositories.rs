//! Real local repository metadata, lifetime and phase-isolated version bindings.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("local-repo-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        f.file("MODULE.bazel", "module(name='main')\n");
        f.file("BUILD.bazel", "");
        f
    }
    fn file(&self, name: &str, contents: &str) {
        fs::write(self.0.path().join(name), contents).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new("timeout")
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .current_dir(self.0.path())
            .output()
            .unwrap()
    }
    fn build(&self, targets: &[&str]) -> (Vec<(PathBuf, Vec<u8>)>, String) {
        let mut args = vec!["build"];
        args.extend_from_slice(targets);
        let result = self.run(&args);
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(result.status.success(), "{stderr}");
        let outputs = String::from_utf8(result.stdout)
            .unwrap()
            .lines()
            .map(|line| {
                let path = PathBuf::from(line.split_once('\t').unwrap().1);
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        (outputs, stderr)
    }
    fn reject(&self, args: &[&str], expected: &str) {
        let result = self.run(args);
        assert_eq!(
            result.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn local_repository_transitive_version_bindings_are_phase_isolated() {
    let f = Fixture::new();
    f.file("helper.bzl", "observed=getattr(native,'bazel_version','absent')\ndef current():\n    return getattr(native,'bazel_version','absent')\n");
    f.file("repo.bzl", "load(':helper.bzl','observed','current')\ndef _impl(ctx):\n    ctx.file('BUILD.bazel',\"exports_files(['value'])\\n\")\n    ctx.file('value', observed+'|'+current()+'|'+native.bazel_version+'\\n')\nr=repository_rule(_impl,local=True)\n");
    f.file(
        "MODULE.bazel",
        "module(name='main')\nr=use_repo_rule('//:repo.bzl','r')\nr(name='made')\n",
    );
    f.file("BUILD.bazel", "load(':helper.bzl','observed','current')\ngenrule(name='copy',srcs=['@made//:value'],outs=['copy.txt'],cmd=\"cat $(SRCS) > $@; printf '%s' 'BUILD=\"+observed+'|'+current()+\"' >> $@\")\n");
    let (outputs, _) = f.build(&["//:copy"]);
    assert_eq!(outputs[0].1, b"9.3.0|9.3.0|9.3.0\nBUILD=absent|absent");
    // Reverse load order with the same RepositorySet, not merely fresh CLI state.
    let repos = std::sync::Arc::new(bazel_rs::repository::RepositorySet::new(
        f.0.path().to_owned(),
    ));
    let label = bazel_rs::label::Label::parse_canonical("//:helper.bzl").unwrap();
    for (globals, expected) in [
        (bazel_rs::package::module_bzl_globals(), "9.3.0"),
        (bazel_rs::package::bzl_globals(), "absent"),
        (bazel_rs::package::module_bzl_globals(), "9.3.0"),
    ] {
        let loaded =
            bazel_rs::loading::load_definition_in(repos.clone(), &label, "observed", &globals)
                .unwrap();
        assert_eq!(loaded.value.value().unpack_str(), Some(expected));
    }
}

#[test]
fn local_repository_cold_warm_callbacks_execute_once_per_demanded_repository() {
    for option in [
        "",
        ", local=False",
        ", local=True",
        ", configure=True",
        ", local=True, configure=True",
    ] {
        let f = Fixture::new();
        f.file("repo.bzl",&format!("def _impl(ctx):\n    print('LOCAL_CALLBACK_EXECUTED')\n    ctx.file('BUILD.bazel',\"exports_files(['one','two'])\\n\")\n    ctx.file('one',native.bazel_version)\n    ctx.file('two',native.bazel_version)\nr=repository_rule(_impl{option})\n"));
        f.file(
            "MODULE.bazel",
            "module(name='main')\nr=use_repo_rule('//:repo.bzl','r')\nr(name='made')\n",
        );
        let (first, stderr) = f.build(&["@made//:one", "@made//:two"]);
        assert_eq!(stderr.matches("LOCAL_CALLBACK_EXECUTED").count(), 1);
        let (second, stderr) = f.build(&["@made//:one", "@made//:two"]);
        // N61: a warm invocation reuses the verified publication without
        // re-running the rule — EXCEPT local=True repositories, which always
        // execute again (RepositoryFetchFunction.shouldUseCachedRepoContents:
        // local repositories never use cached contents while fetching is
        // enabled, and a native invocation is a fresh server).
        let warm = if option.contains("local=True") { 1 } else { 0 };
        assert_eq!(stderr.matches("LOCAL_CALLBACK_EXECUTED").count(), warm);
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|(_, bytes)| bytes == b"9.3.0"));
    }
}

#[test]
fn configure_repository_rule_from_extension_builds_like_rules_cc_autoconf() {
    // rules_cc cc_configure.bzl shape: environ + configure=True, created by a
    // module extension. configure only affects fetch --configure and
    // vendoring in Bazel, so an ordinary build behaves like any repository.
    let f = Fixture::new();
    f.file("repo.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel',\"exports_files(['one'])\\n\")\n    ctx.file('one','configured')\nr=repository_rule(environ=['BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN'],implementation=_impl,configure=True)\n");
    f.file(
        "ext.bzl",
        "load(':repo.bzl','r')\ndef _impl(ctx):\n    r(name='made')\ne=module_extension(_impl)\n",
    );
    f.file(
        "MODULE.bazel",
        "module(name='main')\nx=use_extension('//:ext.bzl','e')\nuse_repo(x,'made')\n",
    );
    let (first, _) = f.build(&["@made//:one"]);
    let (second, _) = f.build(&["@made//:one"]);
    assert_eq!(first, second);
    assert!(first.iter().all(|(_, bytes)| bytes == b"configured"));
}

#[test]
fn local_repository_failure_is_lazy_and_never_published() {
    let f = Fixture::new();
    f.file("repo.bzl","def _impl(ctx):\n    ctx.file('partial','not published')\n    fail('LOCAL_CALLBACK_FAILURE')\nr=repository_rule(_impl,local=True)\n");
    f.file(
        "MODULE.bazel",
        "module(name='main')\nr=use_repo_rule('//:repo.bzl','r')\nr(name='unused')\n",
    );
    f.file("input", "okay");
    f.file("BUILD.bazel", "filegroup(name='data',srcs=['input'])\n");
    let (_, stderr) = f.build(&["//:data"]);
    assert!(!stderr.contains("LOCAL_CALLBACK_FAILURE"));
    f.reject(&["query", "@unused//:all"], "LOCAL_CALLBACK_FAILURE");
    for name in ["trees", "manifests", "stages"] {
        let path = f.0.path().join("cache/repositories/direct").join(name);
        assert!(fs::read_dir(path).unwrap().next().is_none());
    }
    // Only the persistent deterministic stage lock may remain.
    for entry in fs::read_dir(f.0.path().join("cache/repositories/direct/locks")).unwrap() {
        assert!(
            entry
                .unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .starts_with("stage-")
        );
    }
}

#[test]
fn local_repository_version_does_not_inject_native_into_top_level_files() {
    let f = Fixture::new();
    f.file("BUILD.bazel", "observed=native.bazel_version\n");
    f.reject(&["query", "//:all"], "native");
    f.file("BUILD.bazel", "");
    f.file(
        "MODULE.bazel",
        "module(name='main')\nobserved=native.bazel_version\n",
    );
    // Root recording rejects both local and external demand; retain both scopes.
    f.reject(&["query", "//:all"], "native");
    f.reject(&["build", "//:all"], "native");
    f.reject(&["query", "@missing//:all"], "native");
}

#[test]
fn local_repository_compatibility_target_agrees_with_real_cli_and_tools() {
    let f = Fixture::new();
    let version = f.run(&["version"]);
    assert!(version.status.success());
    assert!(
        String::from_utf8(version.stdout)
            .unwrap()
            .contains(&format!(
                "experimental; compatibility target Bazel {}",
                bazel_rs::COMPATIBILITY_VERSION
            ))
    );
    let output = f.run(&["capabilities"]);
    assert!(output.status.success());
    let caps: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        caps["compatibility_target"],
        bazel_rs::COMPATIBILITY_VERSION
    );
    assert_eq!(caps["full_feature_parity"], false);
    assert_eq!(
        bazel_rs::builtin_tools::VERSION,
        bazel_rs::COMPATIBILITY_VERSION
    );
}
