use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-repositories-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let fixture = Self(path);
        fixture.file("MODULE.bazel", "module(name='main_module')\nbazel_dep(name='alpha',version='1',repo_name='first')\nbazel_dep(name='beta',version='1',repo_name='second')\nlocal_path_override(module_name='alpha',path='repos/alpha')\nlocal_path_override(module_name='beta',path='repos/beta')\n");
        fixture.file("BUILD.bazel", "");
        fixture.file("repos/alpha/MODULE.bazel", "module(name='alpha',version='1')\nbazel_dep(name='beta',version='1',repo_name='dep')\n");
        fixture.file(
            "repos/beta/MODULE.bazel",
            "module(name='beta',version='1')\n",
        );
        for (prefix, content) in [
            ("", "main"),
            ("repos/alpha/", "alpha"),
            ("repos/beta/", "beta"),
        ] {
            fixture.file(&format!("{prefix}p/BUILD.bazel"), "package(default_visibility=['//visibility:public'])\nexports_files(['data.txt'])\nfilegroup(name='files',srcs=['data.txt'])\ngenrule(name='copy',srcs=['data.txt'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\n");
            fixture.file(&format!("{prefix}p/data.txt"), content);
        }
        fixture
    }
    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
fn files(output: &Output) -> Vec<(String, PathBuf)> {
    assert!(output.status.success(), "{}", stderr(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (label, path) = line.split_once('\t').unwrap();
            (label.to_owned(), PathBuf::from(path))
        })
        .collect()
}

#[test]
fn identical_packages_in_three_repositories_have_distinct_outputs_and_cache_keys() {
    let fixture = Fixture::new();
    let output = fixture.run(&[
        "build",
        "--jobs=4",
        "//p:copy",
        "@first//p:copy",
        "@second//p:copy",
    ]);
    let outputs = files(&output);
    assert_eq!(outputs.len(), 3);
    for (label, path) in &outputs {
        let expected = if label.starts_with("@@alpha+") {
            b"alpha".as_slice()
        } else if label.starts_with("@@beta+") {
            b"beta"
        } else {
            b"main"
        };
        assert_eq!(fs::read(path).unwrap(), expected);
    }
    assert_ne!(outputs[0].1, outputs[1].1);
    assert!(stderr(&output).contains("3 actions executed"));
    let warm = fixture.run(&["build", "//p:copy", "@first//p:copy", "@second//p:copy"]);
    assert_eq!(files(&warm), outputs);
    assert!(stderr(&warm).contains("0 actions executed, 3 cache hits"));
    fixture.file("repos/alpha/p/data.txt", "changed alpha");
    let changed = fixture.run(&["build", "//p:copy", "@first//p:copy", "@second//p:copy"]);
    assert!(stderr(&changed).contains("1 actions executed, 2 cache hits"));
    let changedfiles = files(&changed);
    assert_eq!(
        fs::read(
            &changedfiles
                .iter()
                .find(|(label, _)| label.starts_with("@@alpha+"))
                .unwrap()
                .1
        )
        .unwrap(),
        b"changed alpha"
    );
}

#[test]
fn external_patterns_and_repository_scoped_aliases_do_not_leak() {
    let fixture = Fixture::new();
    fixture.file(
        "repos/alpha/BUILD.bazel",
        "filegroup(name='transitive',srcs=['@dep//p:files'],visibility=['//visibility:public'])\n",
    );
    let output = fixture.run(&["build", "@first//:transitive"]);
    assert_eq!(fs::read(&files(&output)[0].1).unwrap(), b"beta");
    for pattern in ["@first//p:all", "@@alpha+//p/...", "@first//..."] {
        let output = fixture.run(&["query", pattern]);
        assert!(output.status.success(), "{}", stderr(&output));
        let labels = String::from_utf8_lossy(&output.stdout);
        assert!(labels.contains("@@alpha+//p:copy\n"));
        assert!(labels.lines().all(|label| label.starts_with("@@alpha+//")));
    }
    let unknown = fixture.run(&["query", "@dep//p:files"]);
    assert!(!unknown.status.success());
    assert!(unknown.stdout.is_empty());
    let root = fixture.run(&["query", "//..."]);
    assert!(root.status.success(), "{}", stderr(&root));
    assert!(!String::from_utf8_lossy(&root.stdout).contains("@@alpha"));
}

#[test]
fn same_package_name_does_not_bypass_cross_repository_visibility() {
    let fixture = Fixture::new();
    fixture.file("repos/alpha/p/BUILD.bazel","filegroup(name='secret',srcs=['data.txt'])\nfilegroup(name='local',srcs=[':secret'],visibility=['//visibility:public'])\n");
    fixture.file(
        "p/BUILD.bazel",
        "filegroup(name='bad',srcs=['@first//p:secret'])\n",
    );
    let denied = fixture.run(&["build", "//p:bad"]);
    assert!(!denied.status.success());
    assert!(stderr(&denied).contains("not visible"));
    assert!(!fixture.0.join("bazel-rs-out").exists());
    assert_eq!(
        fs::read(&files(&fixture.run(&["build", "@first//p:local"]))[0].1).unwrap(),
        b"alpha"
    );
    fixture.file(
        "repos/alpha/p/BUILD.bazel",
        "filegroup(name='secret',srcs=['data.txt'],visibility=['//p:__pkg__'])\n",
    );
    assert!(!fixture.run(&["build", "//p:bad"]).status.success());
    fixture.file(
        "repos/alpha/p/BUILD.bazel",
        "filegroup(name='secret',srcs=['data.txt'],visibility=['@@//p:__pkg__'])\n",
    );
    assert_eq!(
        fs::read(&files(&fixture.run(&["build", "//p:bad"]))[0].1).unwrap(),
        b"alpha"
    );
}

#[test]
fn external_provider_sources_and_lexical_labels_keep_real_paths_and_identity() {
    let fixture = Fixture::new();
    fixture.file("repos/alpha/BUILD.bazel", "load(':defs.bzl','provide')\nprovide(name='provided',visibility=['//visibility:public'])\n");
    fixture.file("repos/alpha/defs.bzl",r#"
Info=provider(fields=['file'])
def _provide(ctx):
    return [Info(file=ctx.file.src),DefaultInfo(files=depset([]))]
provide=rule(implementation=_provide,attrs={'src':attr.label(default='//p:data.txt',allow_single_file=True)})
def _consume(ctx):
    src=ctx.attr.dep[Info].file
    out=ctx.actions.declare_file('dir/result.txt')
    sibling=ctx.actions.declare_file('metadata.txt',sibling=out)
    ctx.actions.run_shell(inputs=[src],outputs=[out],command='cat "$1" > "$2"',arguments=[src.path,out.path])
    ctx.actions.write(sibling,src.path+'|'+src.short_path+'|'+str(src.owner)+'|'+str(Label('//p:data.txt'))+'|'+str(ctx.label))
    return [DefaultInfo(files=depset([out,sibling]))]
consume=rule(implementation=_consume,attrs={'dep':attr.label(mandatory=True)})
"#);
    fixture.file(
        "BUILD.bazel",
        "load('@first//:defs.bzl','consume')\nconsume(name='consume',dep='@first//:provided')\n",
    );
    let output = fixture.run(&["build", "//:consume"]);
    let outputs = files(&output);
    assert_eq!(fs::read(&outputs[0].1).unwrap(), b"alpha");
    assert_eq!(fs::read(&outputs[1].1).unwrap(),b"external/alpha+/p/data.txt|../alpha+/p/data.txt|@@alpha+//p:data.txt|@@alpha+//p:data.txt|@@//:consume");
    fixture.file("repos/alpha/BUILD.bazel","load(':defs.bzl','provide','consume')\nprovide(name='provided')\nconsume(name='consume',dep=':provided',visibility=['//visibility:public'])\n");
    let external = files(&fixture.run(&["build", "@first//:consume"]));
    assert!(
        external[0]
            .1
            .ends_with("bin/external/alpha+/dir/result.txt")
    );
    assert_eq!(fs::read(&external[0].1).unwrap(), b"alpha");
    assert!(
        fs::read_to_string(&external[1].1)
            .unwrap()
            .ends_with("@@alpha+//:consume")
    );
}

#[test]
fn explicit_main_labels_and_missing_empty_alias_are_distinct() {
    let fixture = Fixture::new();
    fixture.file(
        "repos/alpha/BUILD.bazel",
        "filegroup(name='main',srcs=['@@//p:files'],visibility=['//visibility:public'])\n",
    );
    assert_eq!(
        fs::read(&files(&fixture.run(&["build", "@first//:main"]))[0].1).unwrap(),
        b"main"
    );
    fixture.file(
        "repos/alpha/BUILD.bazel",
        "filegroup(name='bad',srcs=['@//p:files'])\n",
    );
    let output = fixture.run(&["build", "@first//:bad"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn unused_module_resolution_remains_lazy_but_requested_failures_do_not_fallback() {
    let fixture = Fixture::new();
    fixture.file(
        "MODULE.bazel",
        "module(name='main_module')\nbazel_dep(name='not_downloaded',version='1')\n",
    );
    assert_eq!(
        fs::read(&files(&fixture.run(&["build", "//p:files"]))[0].1).unwrap(),
        b"main"
    );
    assert!(
        !fixture
            .run(&["query", "@not_downloaded//p:all"])
            .status
            .success()
    );
    assert!(!fixture.0.join("MODULE.bazel.lock").exists());
}

#[test]
fn registry_local_source_uses_selected_metadata_and_configured_base_path() {
    let fixture = Fixture::new();
    fixture.file(
        "MODULE.bazel",
        "module(name='main_module')\nbazel_dep(name='alpha',version='1',repo_name='first')\n",
    );
    // The source MODULE still mentions beta. Registry metadata, not that file,
    // determines the dependency graph, and only the selected source is read.
    fixture.file(
        "registry/modules/alpha/1/MODULE.bazel",
        "module(name='alpha',version='1')\n",
    );
    fixture.file(
        "registry/modules/alpha/1/source.json",
        "{\"type\":\"local_path\",\"path\":\"repos/alpha\"}",
    );
    fixture.file(
        "registry/bazel_registry.json",
        "{\"module_base_path\":\"..\"}",
    );
    let registry = format!("--registry=file://{}", fixture.0.join("registry").display());
    let built = fixture.run(&["build", &registry, "@first//p:copy"]);
    assert_eq!(fs::read(&files(&built)[0].1).unwrap(), b"alpha");
    let query = fixture.run(&["query", &registry, "@first//p:all"]);
    assert!(query.status.success(), "{}", stderr(&query));
    assert!(String::from_utf8_lossy(&query.stdout).contains("@@alpha+//p:copy"));
    fixture.file("repos/alpha/p/data.txt", "live local registry source");
    let changed = fixture.run(&["build", &registry, "@first//p:copy"]);
    assert_eq!(
        fs::read(&files(&changed)[0].1).unwrap(),
        b"live local registry source"
    );
    assert!(stderr(&changed).contains("1 actions executed"));
    assert!(!fixture.0.join("MODULE.bazel.lock").exists());
}

#[test]
fn unavailable_native_dependency_is_resolved_only_when_selected() {
    let fixture = Fixture::new();
    fixture.file("repos/alpha/p/BUILD.bazel", "exports_files(['data.txt'])\ngenrule(name='copy',srcs=['data.txt'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\nfilegroup(name='unavailable',srcs=['@not_declared//:file'])\n");
    let query = fixture.run(&["query", "@first//p:all"]);
    assert!(query.status.success(), "{}", stderr(&query));
    assert!(String::from_utf8_lossy(&query.stdout).contains("@@alpha+//p:unavailable"));
    assert_eq!(
        fs::read(&files(&fixture.run(&["build", "@first//p:copy"]))[0].1).unwrap(),
        b"alpha"
    );
    let unavailable = fixture.run(&["build", "@first//p:unavailable"]);
    assert!(!unavailable.status.success());
    assert!(stderr(&unavailable).contains("unknown apparent repository"));
    fixture.file(
        "repos/alpha/p/BUILD.bazel",
        "filegroup(name='unsafe',srcs=['@not_declared//p:../escape'])\n",
    );
    assert!(!fixture.run(&["query", "@first//p:all"]).status.success());
}
