use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Workspace(PathBuf);
impl Workspace {
    fn new(build: &str) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "bazel-rs-integration-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("MODULE.bazel"), "module(name = 'fixture')\n").unwrap();
        fs::write(path.join("BUILD.bazel"), build).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
    fn file(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn stdout(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}
fn stderr(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}
fn output_path(output: &std::process::Output) -> PathBuf {
    let text = stdout(output);
    PathBuf::from(text.lines().next().unwrap().split_once('\t').unwrap().1)
}

#[test]
fn builds_dependency_chain_and_reuses_verified_content_cache() {
    let ws = Workspace::new(include_str!("fixtures/basic/BUILD.bazel"));
    ws.file("input.txt", "hello\n");
    let query = ws.run(&["query", "//:all"]);
    assert!(query.status.success(), "{}", stderr(&query));
    assert_eq!(stdout(&query), "//:copy\n//:inputs\n//:upper\n");
    let first = ws.run(&["build", "//:upper"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(fs::read_to_string(output_path(&first)).unwrap(), "HELLO\n");
    assert!(stderr(&first).contains("2 actions executed, 0 cache hits"));
    let second = ws.run(&["build", "//:upper"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(stderr(&second).contains("0 actions executed, 2 cache hits"));
    ws.file("input.txt", "changed\n");
    let changed = ws.run(&["build", "//:upper"]);
    assert!(changed.status.success(), "{}", stderr(&changed));
    assert_eq!(
        fs::read_to_string(output_path(&changed)).unwrap(),
        "CHANGED\n"
    );
    assert!(stderr(&changed).contains("2 actions executed, 0 cache hits"));
}

#[test]
fn cycles_and_analysis_errors_do_not_execute_partial_builds() {
    for invalid in [
        "filegroup(name='bad',srcs=[':bad'])",
        "genrule(name='bad',outs=['bad.txt'],cmd='echo $(UNSUPPORTED) > $@')",
        "filegroup(name='bad',srcs=['missing.txt'])",
        "genrule(name='bad',outs=['bad.txt'],cmd='echo \\x00 > $@')",
    ] {
        let ws = Workspace::new(&format!(
            "genrule(name='first',outs=['first.txt'],cmd='echo first > $@')\n{invalid}\n"
        ));
        let result = ws.run(&["build", ":first", ":bad"]);
        assert!(!result.status.success());
        assert!(
            !ws.0.join("bazel-rs-out").exists(),
            "actions ran before analysis completed: {}",
            stderr(&result)
        );
    }
}

#[test]
fn invalid_loading_never_reports_partial_query_success() {
    let ws =
        Workspace::new("filegroup(name='ok')\nload('//:rules.bzl','custom')\ncustom(name='bad')\n");
    let query = ws.run(&["query", "//:all"]);
    assert!(!query.status.success());
    assert!(query.stdout.is_empty());
}

#[test]
fn build_failure_is_nonzero_and_does_not_poison_cache() {
    let ws = Workspace::new(
        "genrule(name='fail',outs=['failure.txt'],cmd='echo partial > $@; exit 7')\n",
    );
    for _ in 0..2 {
        let result = ws.run(&["build", ":fail"]);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
    }
}

#[test]
fn source_order_is_preserved_in_make_variables() {
    let ws = Workspace::new(
        "genrule(name='join',srcs=['z.txt','a.txt'],outs=['joined.txt'],cmd='cat $(SRCS) > $@')\n",
    );
    ws.file("z.txt", "first\n");
    ws.file("a.txt", "second\n");
    let result = ws.run(&["build", ":join"]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert_eq!(
        fs::read_to_string(output_path(&result)).unwrap(),
        "first\nsecond\n"
    );
}

#[test]
fn private_dependencies_are_rejected_and_public_dependencies_work() {
    let ws = Workspace::new("filegroup(name='use',srcs=['//dep:source'])\n");
    ws.file("dep/in.txt", "data");
    ws.file(
        "dep/BUILD.bazel",
        "filegroup(name='source',srcs=['in.txt'])\n",
    );
    let result = ws.run(&["build", ":use"]);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("not visible"));
    ws.file(
        "dep/BUILD.bazel",
        "filegroup(name='source',srcs=['in.txt'],visibility=['//visibility:public'])\n",
    );
    let result = ws.run(&["build", ":use"]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert_eq!(fs::read_to_string(output_path(&result)).unwrap(), "data");
}

#[test]
fn flags_test_command_and_rc_are_not_silently_ignored() {
    let ws = Workspace::new("filegroup(name='ok')\n");
    for args in [
        &["test", ":ok"][..],
        &["build", "--remote_cache=http://example.invalid", ":ok"],
        // SkyQuery and the xml output formatter are not implemented.
        &["query", "--universe_scope=//...", "deps(:ok)"],
        &["query", "--output=xml", ":ok"],
    ] {
        assert!(!ws.run(args).status.success());
    }
    ws.file(".bazelrc", "build --config=production\n");
    let result = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .args(["--nosystem_rc", "--nohome_rc", "build", ":ok"])
        .env_remove("BAZELRC")
        .current_dir(&ws.0)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains("Config value 'production' is not defined in any .rc file"),
        "{}",
        stderr(&result)
    );
    assert!(ws.run(&["query", ":ok"]).status.success());
}

#[test]
fn artifact_paths_cannot_cross_package_boundaries() {
    // Only BUILD files make packages: Bazel 9.3 has no nested repositories
    // (LocalRepositoryLookupFunction.java:27-35), so MODULE.bazel, REPO.bazel
    // and WORKSPACE files below a package are ordinary files.
    for marker in ["BUILD.bazel", "MODULE.bazel", "REPO.bazel", "WORKSPACE"] {
        let crosses = marker == "BUILD.bazel";
        let ws = Workspace::new("genrule(name='bad',outs=['sub/file'],cmd='echo bad > $@')\n");
        ws.file(&format!("sub/{marker}"), "");
        ws.file("sub/input.txt", "nested");
        let result = ws.run(&["build", ":bad"]);
        assert_eq!(
            result.status.success(),
            !crosses,
            "output below {marker}: {}",
            stderr(&result)
        );
        if crosses {
            assert!(!ws.0.join("bazel-rs-out").exists());
        }
        ws.file(
            "BUILD.bazel",
            "filegroup(name='bad',srcs=['sub/input.txt'])\n",
        );
        let result = ws.run(&["build", ":bad"]);
        assert_eq!(
            result.status.success(),
            !crosses,
            "source below {marker}: {}",
            stderr(&result)
        );
    }
}

#[test]
fn distinct_artifacts_with_the_same_execution_path_are_rejected() {
    let ws = Workspace::new(
        "genrule(name='make',outs=['x'],cmd='echo generated > $@')\nfilegroup(name='ambiguous',srcs=[':make','bazel-out/rust-fastbuild/bin/x'])\n",
    );
    ws.file("bazel-out/rust-fastbuild/bin/x", "different source");
    let result = ws.run(&["build", ":ambiguous"]);
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains("different artifacts"),
        "{}",
        stderr(&result)
    );
    assert!(!ws.0.join("bazel-rs-out").exists());
}

#[test]
fn genrule_shell_failures_are_not_masked_by_later_success() {
    for command in ["false; echo masked > $@", "false | cat > $@"] {
        let ws = Workspace::new(&format!(
            "genrule(name='bad',outs=['out'],cmd='{command}')\n"
        ));
        let result = ws.run(&["build", ":bad"]);
        assert!(!result.status.success(), "accepted {command}");
    }
}

#[test]
fn arbitrary_workspace_files_do_not_become_declared_source_targets() {
    let ws = Workspace::new("filegroup(name='files',srcs=['declared.txt'])\n");
    ws.file("declared.txt", "declared");
    ws.file("unlisted.txt", "unlisted");
    assert!(ws.run(&["query", ":declared.txt"]).status.success());
    let result = ws.run(&["query", ":unlisted.txt"]);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("not declared"));
}

#[test]
fn output_prefix_conflicts_across_packages_fail_before_execution() {
    let ws = Workspace::new("genrule(name='gen',outs=['sub'],cmd='echo parent > $@')\n");
    ws.file(
        "sub/BUILD.bazel",
        "genrule(name='gen',outs=['out'],cmd='echo child > $@')\n",
    );
    for labels in [["//:gen", "//sub:gen"], ["//sub:gen", "//:gen"]] {
        let result = ws.run(&["build", "--jobs=4", labels[0], labels[1]]);
        assert!(!result.status.success());
        assert!(stderr(&result).contains("overlapping output paths"));
        assert!(!ws.0.join("bazel-rs-out").exists());
    }
    ws.file("BUILD.bazel", "genrule(name='gen',outs=['sub'],cmd='echo parent > $@')\nfilegroup(name='reverse',srcs=['//sub:gen',':gen'])\n");
    ws.file("sub/BUILD.bazel", "genrule(name='gen',outs=['out'],cmd='echo child > $@',visibility=['//visibility:public'])\n");
    let result = ws.run(&["build", ":reverse"]);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("overlapping output paths"));
    assert!(!ws.0.join("bazel-rs-out").exists());
}

#[test]
fn generated_directories_do_not_conflict_with_source_files() {
    let ws = Workspace::new("genrule(name='gen',outs=['plain/out'],cmd='echo generated > $@')\n");
    ws.file("plain", "source is a file not a directory");
    assert!(ws.run(&["query", "//:all"]).status.success());
    let result = ws.run(&["build", ":gen"]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert_eq!(
        fs::read_to_string(output_path(&result)).unwrap(),
        "generated\n"
    );
}

#[test]
fn plural_locations_are_sorted_unlike_srcs_declaration_order() {
    let ws = Workspace::new(
        "filegroup(name='files',srcs=['z','a'])\ngenrule(name='gen',srcs=[':files'],outs=['out'],cmd='cat $(locations :files) > $@')\n",
    );
    ws.file("a", "A");
    ws.file("z", "Z");
    let result = ws.run(&["build", ":gen"]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert_eq!(fs::read_to_string(output_path(&result)).unwrap(), "AZ");
}
