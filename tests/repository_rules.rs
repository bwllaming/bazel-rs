//! Real CLI scenarios for the bounded direct repository-rule executor.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("direct-repo-cli-")
                .tempdir_in(crate::test_temp::root())
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
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .current_dir(self.0.path())
            .explicit_modules();
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    /// Run with an exact client environment (Bazel `--client_env`).
    fn run_env(&self, args: &[&str], environment: &[(&str, String)]) -> Output {
        let mut command = Command::new("/usr/bin/timeout");
        command
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .envs(verifier_environment())
            .env("LC_ALL", "C");
        for (name, value) in environment {
            command.env(name, value);
        }
        command.explicit_modules().output().unwrap()
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
    // Every module sees @bazel_tools (Bazel 9.3 ModuleThreadContext.java:383-401);
    // this profile has no built-in contents, so demanding it fails explicitly.
    let unknown = f.run(&["query", "@ordinary//:data"]);
    rejected(&unknown, "built-in repository @@bazel_tools");
}

#[test]
fn demanded_definition_initialization_has_no_step_limit() {
    // Bazel bounds no Starlark steps (the former native cap was 1 M ticks).
    let f = Fixture::new();
    f.file("helper.bzl", "def burn():\n    x = 0\n    for i in range(3000000):\n        x = (x + 1) % 17\n    return x\nv = burn()\ndef greeting(x):\n    return x\n");
    let output = f.run(&["query", "@made//:data.txt"]);
    assert!(output.status.success(), "{}", text(&output));
}

#[test]
fn frozen_private_helpers_real_query_build_and_fresh_process_inputs() {
    let f = Fixture::new();
    let query = f.run(&["query", "@made//:data.txt"]);
    assert!(query.status.success(), "{}", text(&query));
    assert_eq!(
        String::from_utf8(query.stdout).unwrap(),
        "@made//:data.txt\n"
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
fn label_template_repositories_keep_their_pre_n21_publication_identity() {
    // Goldens were produced by the main binary (040d055, before N21) with a
    // private --cache-dir on linux/amd64; N21 must not rekey repositories that
    // only use ctx.file and Label templates (direct and extension-generated).
    // N51 intentionally rekeyed both: every repository mapping now includes
    // the built-in @bazel_tools entry (Bazel 9.3 ModuleThreadContext.java:383-401).
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\nmake = use_repo_rule('//:defs.bzl','make')\nmake(name='made', source='//:data.tpl')\n");
    f.file("BUILD.bazel", "genrule(name='copy',srcs=['@made//:data.txt','@made//:tool.sh'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\n");
    f.file("data.tpl", b"A B AB\r\n\xff");
    f.file("tool.tpl", "#!/bin/sh\necho %{name}\n");
    f.file("defs.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt','tool.sh','plain.txt'])\\n\")\n    ctx.file('plain.txt', 'plain', executable=False)\n    ctx.template('data.txt', ctx.attr.source, {'A':'B'}, executable=False)\n    ctx.template('tool.sh', Label('//:tool.tpl'), {'%{name}':'x'})\nmake = repository_rule(_impl, attrs={'source':attr.label()})\n");
    let identity = |f: &Fixture| {
        let roots = f.roots();
        assert_eq!(roots.len(), 1, "{roots:?}");
        roots[0].file_name().unwrap().to_str().unwrap().to_owned()
    };
    let golden = cfg!(all(target_os = "linux", target_arch = "x86_64"));
    assert_eq!(
        bytes(&f.build("//:copy")[0].1),
        b"B B BB\r\n\xff#!/bin/sh\necho x\n"
    );
    if golden {
        assert_eq!(
            identity(&f),
            "8cc88f0fed662a69614784526c4b0d4f94024b376009932143424822113bae3d"
        );
    }
    // Template content changes still rekey through the templates entry.
    let first = identity(&f);
    f.file("tool.tpl", "#!/bin/sh\necho changed %{name}\n");
    fs::remove_dir_all(f.0.path().join("cache")).unwrap();
    assert_eq!(
        bytes(&f.build("//:copy")[0].1),
        b"B B BB\r\n\xff#!/bin/sh\necho changed x\n"
    );
    assert_ne!(identity(&f), first);
    f.file("tool.tpl", "#!/bin/sh\necho %{name}\n");
    fs::remove_dir_all(f.0.path().join("cache")).unwrap();
    f.file(
        "MODULE.bazel",
        "module(name='main')\next = use_extension('//:ext.bzl','ext')\nuse_repo(ext,'made')\n",
    );
    f.file("ext.bzl", "load(':defs.bzl','make')\ndef _impl(ctx):\n    make(name='made', source='//:data.tpl')\next = module_extension(_impl)\n");
    f.build("//:copy");
    if golden {
        // N52: with a lockfile (the default `update` mode) an extension
        // result may come from MODULE.bazel.lock, so generated repositories
        // are keyed by what the result determines (specs, mapping, sources),
        // not by the evaluation's bound tags or recorded inputs. Re-pinned
        // for integration-16: N51 adds @@bazel_tools to every module's
        // mapping, which changes the mapping component of the key (N52's
        // branch pin was 550c5b0f…; i15's evaluation-keyed pin was
        // ea2fc9f3…).
        assert_eq!(
            identity(&f),
            "d0e5e900b0a9e707a57f586101dfb0675219c692d4e17bc93c24d715c14665ea"
        );
    }
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
    // A download that fails at once and never leaves the machine (no DNS
    // query, as `example.invalid` still needs): a loopback port nothing
    // listens on any more.
    let refused = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/x", listener.local_addr().unwrap())
    };
    for (operation, marker) in [
        (
            "fail('rollback-sentinel')".to_owned(),
            "rollback-sentinel".to_owned(),
        ),
        (
            "ctx.file('../outside', 'bad')".to_owned(),
            "path".to_owned(),
        ),
        (
            "ctx.watch_tree('x')".to_owned(),
            "can't call watch_tree() on non-directory".to_owned(),
        ),
        // A pending download's failure surfaces from wait() (java:640-697).
        (
            format!("ctx.download('{refused}','x',sha256='0'*64,block=False).wait()"),
            format!("Error downloading [{refused}]"),
        ),
        (
            "ctx.symlink('a','../outside')".to_owned(),
            "Cannot write outside of the repository directory".to_owned(),
        ),
        (
            "ctx.execute(['sh','-c','echo should-not-publish'])\n    fail('after-execute')"
                .to_owned(),
            "after-execute".to_owned(),
        ),
        // RepositoryFetchFunction.java:681-695 accepts string-keyed dicts.
        (
            "return {1:'value'}".to_owned(),
            "got dict<int, string> for 'return value', want dict<string, unknown>".to_owned(),
        ),
    ] {
        let f = Fixture::new();
        f.file("defs.bzl", format!("def _impl(ctx):\n    ctx.file('partial', 'private')\n    {operation}\nmake=repository_rule(_impl,attrs={{'text':attr.string()}})\n"));
        rejected(&f.run(&["build", "@made//:data.txt"]), &marker);
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
    rejected(&f.run(&["build", "//:copy"]), "name 'chr' is not defined");
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

include!("support/executable_script.rs");
include!("support/verify_env.rs");

/// A fake tool the CLI under test executes; never `fs::write`n here, because a
/// process another test thread spawns meanwhile would inherit the descriptor
/// and make that exec fail with ETXTBSY.
fn tool(path: &Path, script: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_executable_script(path, script, 0o755);
}

const CC_CONFIGURE: &str = r##"def _impl(ctx):
    env = ctx.os.environ
    if env.get("BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN") == "1":
        ctx.file("BUILD.bazel", "exports_files(['data.txt'])\n")
        ctx.file("data.txt", "disabled\n", executable = False)
        return
    cc = ctx.which(env.get("CC", "gcc"))
    if cc == None:
        fail("Cannot find gcc")
    result = ctx.execute([cc, "--version"])
    if result.return_code != 0:
        fail("cc --version failed: " + result.stderr)
    paths = dict([(l, ctx.path(Label(l))) for l in ["//toolchain:BUILD.tpl", "//toolchain:cc_toolchain_config.bzl"]])
    ctx.symlink(paths["//toolchain:cc_toolchain_config.bzl"], "cc_toolchain_config.bzl")
    ctx.template("BUILD.bazel", paths["//toolchain:BUILD.tpl"], {"%{name}": "k8" if ctx.os.arch == "amd64" else ctx.os.arch})
    ctx.file("data.txt", "cc=%s\nversion=%s\n" % (cc.basename, result.stdout.strip()), executable = False)
make = repository_rule(_impl, attrs = {"text": attr.string()}, configure = True, local = True)
"##;

#[test]
fn cc_configure_shaped_rule_runs_real_tools_from_client_path_and_publishes_links() {
    let f = Fixture::new();
    let bin = f.0.path().join("bin");
    tool(&bin.join("gcc"), "#!/bin/sh\necho 'gcc (Fake) 13.2.0'\n");
    tool(
        &bin.join("clang-n21"),
        "#!/bin/sh\necho 'clang version 18.1.0'\n",
    );
    f.file("toolchain/BUILD.bazel", "");
    f.file(
        "toolchain/BUILD.tpl",
        "exports_files(['data.txt'])\n# cpu %{name}\n",
    );
    f.file("toolchain/cc_toolchain_config.bzl", "CONFIG = 1\n");
    f.file("defs.bzl", CC_CONFIGURE);
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let base = [
        ("PATH", path.clone()),
        ("HOME", f.0.path().display().to_string()),
    ];
    let first = outputs(&f.run_env(&["build", "//:copy", "@made//:data.txt"], &base));
    let generated = first
        .iter()
        .find(|(label, _)| label == "@@+make+made//:data.txt")
        .unwrap();
    assert_eq!(bytes(&generated.1), b"cc=gcc\nversion=gcc (Fake) 13.2.0\n");
    let copy = first.iter().find(|(label, _)| label == "//:copy").unwrap();
    assert_eq!(bytes(&copy.1), b"cc=gcc\nversion=gcc (Fake) 13.2.0\n");
    let root = generated.1.parent().unwrap();
    assert_eq!(
        fs::read_link(root.join("cc_toolchain_config.bzl")).unwrap(),
        fs::canonicalize(f.0.path())
            .unwrap()
            .join("toolchain/cc_toolchain_config.bzl")
    );
    assert_eq!(
        bytes(&root.join("BUILD.bazel")),
        b"exports_files(['data.txt'])\n# cpu k8\n"
    );
    // Every invocation re-executes the rule: a changed client environment is
    // observed without any recorded dependency (Bazel refetches local rules).
    let mut clang = base.to_vec();
    clang.push(("CC", "clang-n21".to_owned()));
    let second = outputs(&f.run_env(&["build", "@made//:data.txt"], &clang));
    assert_eq!(
        bytes(&second[0].1),
        b"cc=clang-n21\nversion=clang version 18.1.0\n"
    );
    assert_ne!(second[0].1, generated.1);
    let mut disabled = base.to_vec();
    disabled.push(("BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN", "1".to_owned()));
    let third = outputs(&f.run_env(&["build", "@made//:data.txt"], &disabled));
    assert_eq!(bytes(&third[0].1), b"disabled\n");
    let missing = f.run_env(
        &["build", "@made//:data.txt"],
        &[("PATH", "/n21-missing".to_owned())],
    );
    rejected(&missing, "Cannot find gcc");
}

#[test]
fn sh_config_shaped_rule_reads_bazel_sh_or_searches_path() {
    let f = Fixture::new();
    f.file("defs.bzl", "def _impl(ctx):\n    sh = ctx.os.environ.get('BAZEL_SH')\n    if not sh:\n        found = ctx.which('sh')\n        sh = str(found) if found else 'none'\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt', sh + '\\n', executable=False)\nmake=repository_rule(_impl,attrs={'text':attr.string()},local=True)\n");
    let set = f.run_env(
        &["build", "@made//:data.txt"],
        &[
            ("PATH", "/usr/bin:/bin".to_owned()),
            ("BAZEL_SH", "/opt/n21/bash".to_owned()),
        ],
    );
    assert_eq!(bytes(&outputs(&set)[0].1), b"/opt/n21/bash\n");
    let searched = f.run_env(
        &["build", "@made//:data.txt"],
        &[("PATH", "/n21-missing:/bin".to_owned())],
    );
    assert_eq!(bytes(&outputs(&searched)[0].1), b"/bin/sh\n");
}

#[test]
fn multitool_shaped_rule_decodes_lockfile_read_through_a_label() {
    let f = Fixture::new();
    f.file(
        "multitool.lock.json",
        r#"{"$schema": "https://example.invalid/schema", "tool": {"binaries": [{"kind": "file", "url": "https://example.invalid/tool", "os": "linux", "cpu": "x86_64"}]}}"#,
    );
    f.file("defs.bzl", "def _impl(ctx):\n    lock = json.decode(ctx.read(ctx.attr.lockfile))\n    names = sorted([k for k in lock.keys() if not k.startswith('$')])\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt', ','.join(names) + ':' + lock['tool']['binaries'][0]['url'] + '\\n', executable=False)\nmake=repository_rule(_impl,attrs={'text':attr.string(),'lockfile':attr.label(default='//:multitool.lock.json')})\n");
    assert_eq!(
        bytes(&f.build("@made//:data.txt")[0].1),
        b"tool:https://example.invalid/tool\n"
    );
}

/// N21 publishes `repository_ctx.symlink` links; N19 source resolution follows
/// them when a direct repository's file is an action input.
#[test]
fn direct_repository_source_symlinks_resolve_like_archive_links() {
    let f = Fixture::new();
    f.file("outside.txt", "main\n");
    f.file("BUILD.bazel", "genrule(name='copy',srcs=['@made//:linked.txt'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\ngenrule(name='escape',srcs=['@made//:escape.txt'],outs=['escape.out'],cmd='cat $(SRCS) > $@')\n");
    f.file("defs.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt', 'linked.txt', 'escape.txt'])\\n\")\n    ctx.file('data.txt', 'inside\\n', executable=False)\n    ctx.symlink('data.txt', 'linked.txt')\n    ctx.symlink(ctx.path(Label('//:outside.txt')), 'escape.txt')\nmake=repository_rule(_impl,attrs={'text':attr.string()})\n");
    let built = f.build("//:copy");
    assert_eq!(bytes(&built[0].1), b"inside\n");
    let root = f.roots().pop().unwrap();
    // Links into the repository are replanted relative (docs/REPOSITORY_RULES.md).
    assert_eq!(
        fs::read_link(root.join("linked.txt")).unwrap(),
        Path::new("data.txt")
    );
    // A link into another repository materialized in this invocation (here
    // the main repository) is read through, as Bazel reads it (N26).
    assert_eq!(bytes(&f.build("//:escape")[0].1), b"main\n");
}

/// N19 extracts real tar hard links, but the direct-repository store rejects
/// hard-linked files when it snapshots the stage, so a `repository_ctx.extract`
/// of such an archive fails before publication (docs/REPOSITORY_RULES.md).
#[test]
fn extracted_archive_hard_links_fail_at_the_direct_repository_snapshot() {
    let f = Fixture::new();
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder
        .append_data(&mut header, "top/data.txt", &b"data\n"[..])
        .unwrap();
    let mut header = tar::Header::new_ustar();
    header.set_size(0);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Link);
    header.set_cksum();
    builder
        .append_link(&mut header, "top/hard.txt", "top/data.txt")
        .unwrap();
    f.file("links.tar", builder.into_inner().unwrap());
    f.file("defs.bzl", "def _impl(ctx):\n    ctx.extract(ctx.attr.archive, strip_prefix='top')\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt', 'hard.txt'])\\n\")\nmake=repository_rule(_impl,attrs={'text':attr.string(),'archive':attr.label(default='//:links.tar')})\n");
    rejected(
        &f.run(&["build", "@made//:data.txt"]),
        "hard-linked direct repository files are unsupported",
    );
    assert!(f.roots().is_empty());
}
