//! CLI root recording preflight, not eager graph or repository resolution.
//! NATIVE_BOUNDARY cases protect native laziness; they are not Bazel parity claims.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(module: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("root-module-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        ws.file("MODULE.bazel", module);
        ws.file("BUILD.bazel", "");
        ws.file("defs/BUILD.bazel", "");
        ws.file("defs/check.bzl", DEFS);
        // BUILD has no predeclared print. Import a real .bzl helper so this
        // marker is executable in the supported BUILD namespace on valid roots.
        ws.file("app/BUILD.bazel", "load('//defs:check.bzl','check',print='build_print')\nprint('G_BUILD_LOADED')\ncheck(name='check')\n");
        ws.file(
            "first/BUILD.bazel",
            format!(
                "genrule(name='first',outs=['first.out'],cmd=\"touch '{}'; printf first > $@\")\n",
                ws.path().join("executed-marker").display()
            ),
        );
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, data: impl AsRef<[u8]>) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(self.path(), args)
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.path().join("private-cache").display()
            ))
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }

    fn untouched(&self) {
        for path in [
            "executed-marker",
            "private-cache/actions",
            "private-cache/work",
            "bazel-rs-out",
        ] {
            assert!(!self.path().join(path).exists(), "unexpected {path}");
        }
    }

    fn not_loaded(&self, output: &Output) {
        for marker in [
            "G_BUILD_LOADED",
            "G_BZL_LOADED",
            "G_ANALYSIS_RAN",
            "G_POISON_LOADED",
            "G_CALLBACK_RAN",
        ] {
            assert!(!stderr(output).contains(marker), "{}", stderr(output));
        }
        self.untouched();
    }

    fn local_success(&self) {
        let query = self.run(&["query", "//app:check"]);
        success(&query);
        assert_eq!(query.stdout, b"//app:check\n");
        assert!(stderr(&query).contains("G_BUILD_LOADED"));
        assert!(stderr(&query).contains("G_BZL_LOADED"));
        assert!(!stderr(&query).contains("G_ANALYSIS_RAN"));
        assert!(!stderr(&query).contains("G_POISON_LOADED"));
        self.untouched();
        let build = self.run(&["build", "//app:check"]);
        assert_eq!(content(&build), [b"g-ok\n".to_vec()]);
        assert!(stderr(&build).contains("G_ANALYSIS_RAN"));
        assert!(!stderr(&build).contains("G_POISON_LOADED"));
        assert!(!stderr(&build).contains("G_CALLBACK_RAN"));
        assert!(!self.path().join("executed-marker").exists());
    }
}

const DEFS: &str = r#"
print('G_BZL_LOADED')
def build_print(message):
    print(message)
def _check(ctx):
    print('G_ANALYSIS_RAN')
    out=ctx.actions.declare_file('check.out')
    ctx.actions.write(out,'g-ok\n')
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check)
"#;

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", stderr(output));
}

fn rejected(output: &Output, diagnostic: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stderr(output));
    assert!(
        output.stdout.is_empty(),
        "partial stdout: {:?}",
        output.stdout
    );
    assert!(stderr(output).contains(diagnostic), "{}", stderr(output));
    assert!(!stderr(output).contains("panicked"), "{}", stderr(output));
}

fn content(output: &Output) -> Vec<Vec<u8>> {
    success(output);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (_, path) = line.split_once('\t').expect("native build output path");
            fs::read(PathBuf::from(path)).unwrap()
        })
        .collect()
}

fn invalid_root(source: &str, diagnostic: &str) {
    for args in [
        vec!["query", "//app:check"],
        vec!["build", "//first:first", "//app:check"],
        // Target selection must not hide a root recording failure either.
        vec!["query", "//missing:target"],
        vec!["build", "//first:first", "//missing:target"],
    ] {
        let ws = Workspace::new(&format!("module(name='root_fixture')\n{source}\n"));
        let output = ws.run(&args);
        rejected(&output, diagnostic);
        assert!(
            stderr(&output).contains("MODULE.bazel"),
            "{}",
            stderr(&output)
        );
        ws.not_loaded(&output);
    }
}

#[test]
fn root_module_run_environment_info_rejected_before_local_loading_and_actions() {
    invalid_root("x=RunEnvironmentInfo()", "RunEnvironmentInfo");
}

#[test]
fn root_module_unknown_symbol_rejected_before_local_loading_and_actions() {
    invalid_root("x=UnknownRootSymbol", "UnknownRootSymbol");
}

#[test]
fn root_module_syntax_rejected_before_local_loading_and_actions() {
    invalid_root("x = (", "MODULE.bazel");
}

#[test]
fn root_module_arithmetic_rejected_before_local_loading_and_actions() {
    invalid_root("x=1//0", "zero");
}

#[test]
fn root_module_valid_local_query_and_build_use_real_loading_and_output() {
    let ws = Workspace::new("module(name='root_fixture')\nprint('G_ROOT_ONCE')\n");
    ws.local_success();
    let cold = ws.run(&["query", "//app:check"]);
    success(&cold);
    assert_eq!(stderr(&cold).matches("G_ROOT_ONCE").count(), 1);
    let warm = ws.run(&["build", "//app:check"]);
    assert_eq!(content(&warm), [b"g-ok\n".to_vec()]);
    assert_eq!(stderr(&warm).matches("G_ROOT_ONCE").count(), 1);
    assert!(stderr(&warm).contains("0 actions executed, 1 cache hits"));
}

#[test]
fn root_module_snapshot_reused_for_external_alias_and_build_load_each_command() {
    let ws = Workspace::new(
        "module(name='root_fixture',repo_name='self_alias')\nprint('G_ROOT_ONCE')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path='dep')\n",
    );
    ws.file("dep/MODULE.bazel", "module(name='dep',version='1')\n");
    ws.file("dep/BUILD.bazel", "exports_files(['data'])\n");
    ws.file("dep/data", "external-materialized\n");
    ws.file(
        "dep/check.bzl",
        format!("print('G_EXTERNAL_LOADED')\n{DEFS}"),
    );
    ws.file("app/BUILD.bazel", "load('@alias//:check.bzl','check',print='build_print')\nprint('G_BUILD_LOADED')\ncheck(name='check')\n");
    // Each invocation is a fresh process, not a persistent server/cache claim.
    for _ in 0..2 {
        let query = ws.run(&["query", "@alias//:data"]);
        success(&query);
        assert!(String::from_utf8_lossy(&query.stdout).ends_with("//:data\n"));
        assert_eq!(stderr(&query).matches("G_ROOT_ONCE").count(), 1);
        let build = ws.run(&["build", "//app:check", "@alias//:data"]);
        let bytes = content(&build);
        assert_eq!(bytes.len(), 2);
        assert!(bytes.contains(&b"g-ok\n".to_vec()));
        assert!(bytes.contains(&b"external-materialized\n".to_vec()));
        assert_eq!(stderr(&build).matches("G_ROOT_ONCE").count(), 1);
        assert_eq!(stderr(&build).matches("G_EXTERNAL_LOADED").count(), 1);
    }
}

#[test]
fn root_module_unused_dependency_recording_does_not_resolve_a_registry() {
    // NATIVE_BOUNDARY: Bazel may eagerly resolve its module graph here.
    let ws =
        Workspace::new("module(name='root_fixture')\nbazel_dep(name='unused',version='9.9.9')\n");
    ws.local_success();
    assert!(!ws.path().join("private-cache/repositories").exists());
}

#[test]
fn root_module_unused_extension_and_repo_rule_do_not_load_poisoned_definitions() {
    // NATIVE_BOUNDARY: recording declarations must not load .bzl or run callbacks.
    for declaration in [
        "e=use_extension('//poison:defs.bzl','ext')\nuse_repo(e,'unused')\n",
        "make=use_repo_rule('//poison:defs.bzl','make')\nmake(name='unused')\n",
    ] {
        let ws = Workspace::new(&format!("module(name='root_fixture')\n{declaration}"));
        ws.file("poison/BUILD.bazel", "");
        ws.file(
            "poison/defs.bzl",
            "print('G_POISON_LOADED')\nfail('G_CALLBACK_RAN')\n",
        );
        ws.local_success();
        assert!(!ws.path().join("private-cache/repositories").exists());
    }
}

#[test]
fn root_module_deferred_unicode_source_profile_error_is_not_a_recording_error() {
    // NATIVE_BOUNDARY: same deferred MODULE source profile as repository_rules.rs.
    let ws = Workspace::new(
        "module(name='root_fixture')\nmake=use_repo_rule('//:repo.bzl','make')\nmake(name='made',text=str(len('é')))\n",
    );
    ws.file("repo.bzl", "def _impl(ctx):\n    print('G_CALLBACK_RAN')\n    ctx.file('BUILD.bazel',\"exports_files(['data'])\\n\")\n    ctx.file('data',ctx.attr.text)\nmake=repository_rule(_impl,attrs={'text':attr.string()})\n");
    ws.local_success();
    let demanded = ws.run(&["query", "@made//:data"]);
    rejected(&demanded, "non-ASCII");
    assert!(!stderr(&demanded).contains("G_CALLBACK_RAN"));
}

#[test]
fn root_module_unused_missing_local_override_fails_only_on_external_demand() {
    // NATIVE_BOUNDARY: an unavailable override is metadata until graph demand.
    let ws = Workspace::new(
        "module(name='root_fixture')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path='nonexistent')\n",
    );
    ws.local_success();
    let demanded = ws.run(&["query", "@alias//:data"]);
    rejected(&demanded, "nonexistent");
}

#[test]
fn root_module_missing_is_optional_with_existing_workspace_markers_only() {
    for marker in ["WORKSPACE", "WORKSPACE.bazel", "REPO.bazel"] {
        let ws = Workspace::new("");
        fs::remove_file(ws.path().join("MODULE.bazel")).unwrap();
        ws.file(marker, "");
        // No .bzl or external mapping is required for this legacy root.
        ws.file("app/BUILD.bazel", "filegroup(name='check')\n");
        let query = ws.run(&["query", "//app:check"]);
        success(&query);
        assert_eq!(query.stdout, b"//app:check\n");
        assert!(content(&ws.run(&["build", "//app:check"])).is_empty());
        ws.untouched();
    }
    let ws = Workspace::new("");
    fs::remove_file(ws.path().join("MODULE.bazel")).unwrap();
    let output = ws.run(&["query", "//app:check"]);
    rejected(&output, "no workspace marker");
    ws.not_loaded(&output);
}

#[test]
fn root_module_directory_and_invalid_utf8_are_not_optional_absence() {
    for directory in [false, true] {
        for command in ["query", "build"] {
            let ws = Workspace::new("");
            ws.file("WORKSPACE", "");
            if directory {
                fs::remove_file(ws.path().join("MODULE.bazel")).unwrap();
                fs::create_dir(ws.path().join("MODULE.bazel")).unwrap();
            } else {
                ws.file("MODULE.bazel", [0xff, 0xfe]);
            }
            let output = ws.run(&[command, "//app:check"]);
            rejected(&output, "MODULE.bazel");
            ws.not_loaded(&output);
        }
    }
}

#[test]
fn root_module_nested_cwd_uses_workspace_root_and_current_package() {
    for command in ["query", "build"] {
        let ws = Workspace::new("module(name='root_fixture')\nprint('G_ROOT_ONCE')\n");
        let cwd = ws.path().join("app/nested");
        fs::create_dir_all(&cwd).unwrap();
        // Relative patterns resolve against the working directory below the
        // workspace root, a package or not (TargetPattern.Parser's
        // relativeDirectory, buildtool/AnalysisPhaseRunner.java:554).
        let output = ws.run_in(&cwd, &[command, ":check"]);
        assert!(
            stderr(&output).contains("no such package 'app/nested'"),
            "{}",
            stderr(&output)
        );
        let output = ws.run_in(&ws.path().join("app"), &[command, ":check"]);
        success(&output);
        assert_eq!(stderr(&output).matches("G_ROOT_ONCE").count(), 1);
        if command == "query" {
            assert_eq!(output.stdout, b"//app:check\n");
            // The residue is joined by spaces into one query expression
            // (QueryOptionHelper.java:48), lexed like Bazel's query Lexer.
            let output = ws.run(&[command, "//app:check", "//first:first"]);
            rejected(
                &output,
                "Error while parsing '//app:check //first:first': unexpected token '//first:first' after query expression '//app:check'",
            );
            let output = ws.run(&[command, "'//app:check'"]);
            assert_eq!(output.stdout, b"//app:check\n");
        } else {
            assert_eq!(content(&output), [b"g-ok\n".to_vec()]);
        }
        // Fresh workspace ensures invalid root cannot reuse successful actions.
        let bad = Workspace::new("module(name='root_fixture')\nx=UnknownRootSymbol\n");
        let cwd = bad.path().join("app/nested");
        fs::create_dir_all(&cwd).unwrap();
        let output = bad.run_in(&cwd, &[command, ":check"]);
        rejected(&output, "UnknownRootSymbol");
        bad.not_loaded(&output);
    }
}

#[test]
fn root_module_cli_argument_errors_and_metadata_commands_precede_recording() {
    let ws = Workspace::new("module(name='root_fixture')\nprint('G_ROOT_ONCE')\nx=1//0\n");
    for (args, diagnostic) in [
        (
            vec!["query", "--unsupported-root-module-option"],
            "--unsupported-root-module-option :: Unrecognized option: --unsupported-root-module-option",
        ),
        (
            vec!["build", "--unsupported-root-module-option", "//app:check"],
            "--unsupported-root-module-option :: Unrecognized option: --unsupported-root-module-option",
        ),
        // QueryOptionHelper.java:61-68, before any loading.
        (
            vec!["query"],
            "missing query expression. Type 'bazel help query' for syntax and help",
        ),
        (vec!["build", "--jobs=0", "//app:check"], "positive integer"),
    ] {
        let output = ws.run(&args);
        rejected(&output, diagnostic);
        assert!(!stderr(&output).contains("G_ROOT_ONCE"));
        assert!(!stderr(&output).contains("MODULE.bazel"));
        ws.not_loaded(&output);
    }
    for command in ["version", "capabilities"] {
        let output = ws.run(&[command]);
        success(&output);
        assert!(!output.stdout.is_empty());
        assert!(!stderr(&output).contains("G_ROOT_ONCE"));
        ws.not_loaded(&output);
    }
}

#[test]
fn root_module_mod_graph_still_evaluates_root_once_without_build_loading() {
    let ws = Workspace::new(
        "module(name='root_fixture',repo_name='self_alias')\nprint('G_ROOT_ONCE')\n",
    );
    let output = ws.run(&["mod", "graph", "--output=json"]);
    success(&output);
    let graph: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(graph["apparentName"], "self_alias");
    assert_eq!(stderr(&output).matches("G_ROOT_ONCE").count(), 1);
    ws.not_loaded(&output);
}
