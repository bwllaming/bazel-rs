//! Real CLI scenarios for the bounded direct repository-rule executor.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("direct-repo-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        f.file("MODULE.bazel", "module(name='main')\nmake = use_repo_rule('//:defs.bzl','make')\nmake(name='made', text='Ada')\n");
        f.file("BUILD.bazel", "genrule(name='copy',srcs=['@made//:data.txt'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\n");
        f.file("helper.bzl", "def _private(x):\n    return 'hello ' + x + '\\n'\ndef greeting(x):\n    return _private(x)\n");
        f.file("defs.bzl", "load(':helper.bzl','greeting')\ndef _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt', greeting(ctx.attr.text), executable=False)\nmake = repository_rule(_impl, attrs={'text':attr.string(default='world')})\n");
        f
    }
    fn file(&self, name: &str, data: impl AsRef<[u8]>) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }
    fn command(&self, args: &[&str]) -> Command {
        // A deadlock is a failed scenario, never an accepted cycle diagnostic.
        let mut command = Command::new("timeout");
        command
            .args([
                "30s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .current_dir(self.0.path());
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn build(&self, target: &str) -> Vec<(String, PathBuf)> {
        outputs(&self.run(&["build", target]))
    }
    fn roots(&self) -> Vec<PathBuf> {
        fs::read_dir(self.0.path().join("cache/repositories/direct/trees"))
            .or_else(|_| fs::read_dir(self.0.path().join("cache/direct/trees")))
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().path())
                    .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
                    .collect()
            })
            .unwrap_or_default()
    }
}
fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
fn outputs(output: &Output) -> Vec<(String, PathBuf)> {
    assert!(output.status.success(), "{}", text(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (label, path) = line.split_once('\t').expect("native build output path");
            (label.to_owned(), PathBuf::from(path))
        })
        .collect()
}
fn rejected(output: &Output, marker: &str) {
    assert!(!output.status.success());
    assert_ne!(output.status.code(), Some(124), "request timed out");
    assert!(!text(output).contains("panicked"), "{}", text(output));
    assert!(text(output).contains(marker), "{}", text(output));
    assert!(output.stdout.is_empty());
}
fn bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap()
}

#[test]
fn selected_module_aliases_do_not_force_unrelated_extension_namespace_resolution() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1')\nlocal_path_override(module_name='dep',path='dep')\nx=use_extension('@bazel_tools//:unavailable.bzl','x')\nuse_repo(x,'ordinary')\n");
    f.file("dep/MODULE.bazel", "module(name='dep',version='1')\n");
    f.file("dep/BUILD.bazel", "exports_files(['data'])\n");
    f.file("dep/data", "existing selected source");
    assert_eq!(
        bytes(&f.build("@dep//:data")[0].1),
        b"existing selected source"
    );
    let unknown = f.run(&["query", "@ordinary//:data"]);
    rejected(&unknown, "unknown apparent repository @bazel_tools");
}

#[test]
fn demanded_definition_initialization_is_bounded_before_any_publication() {
    let f = Fixture::new();
    f.file("helper.bzl", "def burn():\n    x = 0\n    for i in range(1000000000):\n        x = (x + 1) % 17\n    return x\nv = burn()\ndef greeting(x):\n    return x\n");
    let output = f.run(&["query", "@made//:data.txt"]);
    rejected(&output, "ticks");
    assert!(f.roots().is_empty());
}

#[test]
fn frozen_private_helpers_real_query_build_and_fresh_process_inputs() {
    let f = Fixture::new();
    let query = f.run(&["query", "@made//:data.txt"]);
    assert!(query.status.success(), "{}", text(&query));
    assert_eq!(
        String::from_utf8(query.stdout).unwrap(),
        "@@+make+made//:data.txt\n"
    );
    assert_eq!(bytes(&f.build("//:copy")[0].1), b"hello Ada\n");
    let generated = f.build("@made//:data.txt");
    assert_eq!(generated[0].0, "@@+make+made//:data.txt");
    assert_eq!(
        fs::metadata(&generated[0].1).unwrap().permissions().mode() & 0o111,
        0
    );
    let root = generated[0].1.parent().unwrap();
    assert_eq!(bytes(&root.join("REPO.bazel")), b"");
    assert!(!root.join("MODULE.bazel").exists());
    f.file(
        "helper.bzl",
        "def greeting(x):\n    return 'changed ' + x\n",
    );
    assert_eq!(bytes(&f.build("//:copy")[0].1), b"changed Ada");
    f.file("MODULE.bazel", "module(name='main')\nmake=use_repo_rule('//:defs.bzl','make')\nmake(name='made',text='Bob')\n");
    assert_eq!(bytes(&f.build("//:copy")[0].1), b"changed Bob");
}

#[test]
fn template_preserves_binary_input_and_uses_ordered_literal_substitutions() {
    let f = Fixture::new();
    f.file("template.dat", b"A B AB\r\n\xff\xc3\xa9");
    f.file("defs.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.template('data.txt', ctx.attr.template, {'A':'B','B':'C','':'X'}, executable=False)\n    ctx.template('data.txt', 'data.txt', {'CC':'D'}, executable=True)\nmake=repository_rule(_impl,attrs={'text':attr.string(),'template':attr.label(default='//:template.dat')})\n");
    let output = f.build("@made//:data.txt");
    assert_eq!(bytes(&output[0].1), b"C C D\r\n\xff\xc3\xa9");
    assert_ne!(
        fs::metadata(&output[0].1).unwrap().permissions().mode() & 0o111,
        0
    );
    f.file("template.dat", b"B\0A");
    assert_eq!(bytes(&f.build("@made//:data.txt")[0].1), b"C\0C");
}

#[test]
fn validates_entire_innate_group_but_does_not_run_unused_implementations() {
    let f = Fixture::new();
    let original = fs::read_to_string(f.0.path().join("MODULE.bazel")).unwrap();
    f.file(
        "MODULE.bazel",
        format!("{original}make(name='unused',unknown='bad')\n"),
    );
    rejected(
        &f.run(&["build", "//:copy"]),
        "unknown repository attribute",
    );
    assert!(f.roots().is_empty());
    f.file(
        "MODULE.bazel",
        format!("{original}make(name='unused',text='fail')\n"),
    );
    f.file("defs.bzl", "def _impl(ctx):\n    if ctx.attr.text == 'fail':\n        fail('unused-implementation-ran')\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt', ctx.attr.text)\nmake=repository_rule(_impl,attrs={'text':attr.string()})\n");
    assert_eq!(bytes(&f.build("//:copy")[0].1), b"Ada");
    assert_eq!(f.roots().len(), 1);
}

#[test]
fn root_and_dependency_own_separate_direct_names_and_label_defaults() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1')\nlocal_path_override(module_name='dep',path='dep')\nmake=use_repo_rule('@dep//:defs.bzl','make')\nmake(name='made',source='//:data.tpl')\n");
    f.file("dep/MODULE.bazel", "module(name='dep',version='1')\nmake=use_repo_rule('//:defs.bzl','make')\nmake(name='made')\n");
    f.file(
        "dep/BUILD.bazel",
        "filegroup(name='files',srcs=['@made//:data.txt'],visibility=['//visibility:public'])\n",
    );
    f.file("data.tpl", "root");
    f.file("dep/data.tpl", "dependency");
    f.file("dep/defs.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.template('data.txt', ctx.attr.source)\nmake=repository_rule(_impl,attrs={'source':attr.label(default=':data.tpl')})\n");
    let root = f.build("@made//:data.txt");
    assert_eq!(root[0].0, "@@+make+made//:data.txt");
    assert_eq!(bytes(&root[0].1), b"root");
    let dep = f.build("@dep//:files");
    assert_eq!(bytes(&dep[0].1), b"dependency");
    let generated = f.build("@@dep++make+made//:data.txt");
    assert_eq!(bytes(&generated[0].1), b"dependency");
}

#[test]
fn generated_definition_and_sibling_template_dependencies_are_real() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\nseed=use_repo_rule('//:seed.bzl','seed')\nseed(name='seed')\nmake=use_repo_rule('@seed//:generated.bzl','make')\nmake(name='made',source='@seed//:template')\n");
    let generated = "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.template('data.txt', ctx.attr.source)\nmake=repository_rule(_impl,attrs={'source':attr.label(mandatory=True)})\n";
    f.file("generated.tpl", generated);
    f.file("seed.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['generated.bzl','template'])\\n\")\n    ctx.template('generated.bzl', Label('//:generated.tpl'))\n    ctx.file('template', 'genuinely generated')\nseed=repository_rule(_impl)\n");
    assert_eq!(bytes(&f.build("//:copy")[0].1), b"genuinely generated");
    assert_eq!(f.roots().len(), 2);
}

#[test]
fn cycles_fail_with_demand_chain_not_timeout_or_panic() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\nmake=use_repo_rule('//:defs.bzl','make')\nmake(name='a',source='@b//:data.txt')\nmake(name='b',source='@a//:data.txt')\n");
    f.file("defs.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.template('data.txt',ctx.attr.source)\nmake=repository_rule(_impl,attrs={'source':attr.label()})\n");
    let output = f.run(&["build", "@a//:data.txt"]);
    rejected(&output, "cycle");
    assert!(text(&output).contains("+make+a") && text(&output).contains("+make+b"));
    assert!(f.roots().is_empty());
}

#[test]
fn callback_failure_unsupported_operations_and_paths_never_publish() {
    for (operation, marker) in [
        ("fail('rollback-sentinel')", "rollback-sentinel"),
        ("ctx.file('../outside', 'bad')", "path"),
        ("ctx.execute(['echo','bad'])", "execute"),
        ("ctx.symlink('a','b')", "symlink"),
        ("return {'suggested':'value'}", "return"),
    ] {
        let f = Fixture::new();
        f.file("defs.bzl", format!("def _impl(ctx):\n    ctx.file('partial', 'private')\n    {operation}\nmake=repository_rule(_impl,attrs={{'text':attr.string()}})\n"));
        rejected(&f.run(&["build", "@made//:data.txt"]), marker);
        assert!(f.roots().is_empty());
        assert!(!f.0.path().join("outside").exists());
        assert!(
            !f.0.path()
                .join("cache/repositories/direct/trees/outside")
                .exists()
        );
    }
}

#[test]
fn direct_source_profile_rejects_hidden_unicode_computation_but_metadata_stays_lazy() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\nmake=use_repo_rule('//:defs.bzl','make')\nmake(name='made',text=str(len('é')))\n");
    assert!(f.run(&["query", "//:copy"]).status.success());
    rejected(&f.run(&["build", "//:copy"]), "non-ASCII");
    assert!(f.roots().is_empty());
    f.file("MODULE.bazel", "module(name='main')\nmake=use_repo_rule('//:defs.bzl','make')\nmake(name='made',text='ASCII')\n");
    f.file(
        "helper.bzl",
        "alias = chr\ndef greeting(x):\n    return str(len(alias(233)))\n",
    );
    rejected(&f.run(&["build", "//:copy"]), "chr references");
    assert!(f.roots().is_empty());
}

#[test]
fn cache_corruption_is_repaired_and_concurrent_cli_publication_is_complete() {
    let f = Fixture::new();
    let source = f.build("@made//:data.txt")[0].1.clone();
    fs::write(&source, b"corrupted").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(bytes(&f.build("@made//:data.txt")[0].1), b"hello Ada\n");
    assert_eq!(
        fs::metadata(&source).unwrap().permissions().mode() & 0o111,
        0
    );
    let mut a = f.command(&["build", "@made//:data.txt"]);
    let mut b = f.command(&["build", "@made//:data.txt"]);
    a.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    b.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    for output in [a.wait_with_output().unwrap(), b.wait_with_output().unwrap()] {
        assert_eq!(bytes(&outputs(&output)[0].1), b"hello Ada\n");
    }
}
