use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new(build: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-parallel-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='parallel_fixture')\n");
        ws.file("BUILD.bazel", build);
        ws
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

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn contents(output: &Output) -> Vec<(String, String)> {
    assert!(output.status.success(), "{}", stderr(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (label, path) = line.split_once('\t').unwrap();
            (label.to_owned(), fs::read_to_string(path).unwrap())
        })
        .collect()
}

const DIAMOND: &str = r#"
genrule(name='root',srcs=['seed'],outs=['z','a'],cmd='cat $< > $(location :z); printf A > $(location :a)')
filegroup(name='forward',srcs=[':root'])
genrule(name='left',srcs=[':forward'],outs=['left.out'],cmd='cat $(SRCS) > $@')
genrule(name='right',srcs=[':a',':z'],outs=['right.out'],cmd='cat $(SRCS) > $@')
filegroup(name='pair',srcs=[':right',':left'])
genrule(name='join',srcs=[':pair','middle',':a'],outs=['joined'],cmd='cat $(locations :pair) $(location middle) $(location :a) > $@')
filegroup(name='final',srcs=[':join',':pair',':a','middle'])
"#;

#[test]
fn parallel_diamond_preserves_output_indices_filegroups_and_make_input_order() {
    let ws = Workspace::new("");
    ws.file("pkg/BUILD.bazel", DIAMOND);
    ws.file("pkg/seed", "Z");
    ws.file("pkg/middle", "M");
    let result = ws.run(&["build", "--jobs=4", "//pkg:final", "//pkg:a"]);
    assert_eq!(
        contents(&result),
        [
            ("//pkg:a".into(), "A".into()),
            // $(locations :pair) sorts left.out before right.out, independently
            // of :pair's srcs order (which the filegroup below still preserves).
            ("//pkg:final".into(), "ZAAZMA".into()),
            ("//pkg:final".into(), "AZ".into()),
            ("//pkg:final".into(), "ZA".into()),
            ("//pkg:final".into(), "A".into()),
            ("//pkg:final".into(), "M".into()),
        ]
    );
    assert!(stderr(&result).contains("4 actions executed, 0 cache hits"));
    let cached = ws.run(&["build", "//pkg:final", "--jobs", "2", "//pkg:a"]);
    assert_eq!(contents(&cached), contents(&result));
    assert_eq!(cached.stdout, result.stdout);
    assert!(stderr(&cached).contains("0 actions executed, 4 cache hits"));
}

#[test]
fn jobs_one_and_parallel_have_identical_outputs_and_share_cache() {
    let ws = Workspace::new(DIAMOND);
    ws.file("seed", "Z");
    ws.file("middle", "M");
    let serial = ws.run(&["build", "--jobs", "1", ":final"]);
    let expected = contents(&serial);
    assert!(stderr(&serial).contains("4 actions executed, 0 cache hits"));
    let parallel = ws.run(&["build", "--jobs=8", ":final"]);
    assert_eq!(contents(&parallel), expected);
    assert_eq!(parallel.stdout, serial.stdout);
    assert!(stderr(&parallel).contains("0 actions executed, 4 cache hits"));
    let fresh = ws.run(&["--cache-dir=other-cache", "build", "--jobs=8", ":final"]);
    assert_eq!(contents(&fresh), expected);
    assert!(stderr(&fresh).contains("4 actions executed, 0 cache hits"));
}

#[test]
fn mixed_generated_and_source_inputs_keep_first_occurrence_order() {
    let ws = Workspace::new(
        "genrule(name='make',outs=['z','a'],cmd='printf Z > $(location :z); printf A > $(location :a)')\n\
         filegroup(name='files',srcs=[':a','source',':z',':a'])\n\
         genrule(name='join',srcs=[':files',':z','last'],outs=['out'],cmd='cat $(SRCS) > $@')\n",
    );
    ws.file("source", "S");
    ws.file("last", "L");
    let result = ws.run(&["build", "--jobs=3", ":join"]);
    assert_eq!(contents(&result), [("//:join".into(), "ASZL".into())]);
}

#[test]
fn jobs_requires_a_positive_integer_and_is_only_a_build_option() {
    let ws = Workspace::new("filegroup(name='ok')\n");
    for value in [
        "",
        "0",
        "-1",
        "+1",
        "1.5",
        "abc",
        " 2",
        "999999999999999999999999999999999999",
    ] {
        let flag = format!("--jobs={value}");
        let result = ws.run(&["build", &flag, ":ok"]);
        assert!(!result.status.success(), "accepted {flag}");
        assert!(
            stderr(&result).contains("positive integer"),
            "{}",
            stderr(&result)
        );
    }
    for args in [
        vec!["build", "--jobs"],
        vec!["build", "--jobs", ":ok"],
        vec!["build", "--jobs=2"],
        vec!["query", "--jobs=2", ":ok"],
        vec!["query", "--jobs=2"],
        vec!["--jobs=2", "build", ":ok"],
        vec!["build", "--jobs=2", "--unknown", ":ok"],
        vec!["build", "--jobs=2", "--ignore_all_rc_files", ":ok"],
        vec!["build", "--jobs=2", "--cache-dir=late", ":ok"],
    ] {
        let result = ws.run(&args);
        assert!(!result.status.success(), "accepted {args:?}");
        assert!(result.stdout.is_empty());
    }
    assert!(ws.run(&["build", "--jobs=1000", ":ok"]).status.success());
    assert!(!ws.0.join("bazel-rs-out").exists());
}

#[test]
fn parallel_preflight_rejects_every_invalid_graph_before_cache_creation() {
    for invalid in [
        "genrule(name='bad',outs=['bad.out'],cmd='echo $(UNKNOWN) > $@')",
        "genrule(name='bad',outs=['bad.out'],cmd='echo \\x00 > $@')",
        "filegroup(name='bad',srcs=['missing'])",
        "filegroup(name='bad',srcs=[':bad'])",
        "genrule(name='bad',outs=['dir','dir/child'],cmd='true')",
        "genrule(name='make',outs=['x'],cmd='printf x > $@')\nfilegroup(name='bad',srcs=[':make','bazel-out/rust-fastbuild/bin/x'])",
    ] {
        let ws = Workspace::new(&format!(
            "genrule(name='aaa',outs=['first'],cmd='printf ran > $@')\n{invalid}\n"
        ));
        ws.file("bazel-out/rust-fastbuild/bin/x", "source collision");
        let result = ws.run(&["build", "--jobs=8", ":aaa", ":bad"]);
        assert!(!result.status.success(), "accepted {invalid}");
        assert!(result.stdout.is_empty());
        assert!(!ws.0.join("bazel-rs-out").exists(), "{}", stderr(&result));
    }
}

#[test]
fn exported_source_visibility_scopes_and_top_level_access() {
    let ws = Workspace::new("");
    ws.file("owner/data", "exported");
    ws.file(
        "owner/BUILD.bazel",
        "exports_files(['data'],visibility=['//team:__subpackages__','//single:__pkg__'])\n",
    );
    for package in [
        "team",
        "team/child",
        "single",
        "single/child",
        "teams",
        "stranger",
    ] {
        ws.file(
            &format!("{package}/BUILD.bazel"),
            "filegroup(name='use',srcs=['//owner:data'])\n",
        );
        let target = format!("//{package}:use");
        let result = ws.run(&["build", "--jobs=4", &target]);
        if ["team", "team/child", "single"].contains(&package) {
            assert_eq!(contents(&result), [(target, "exported".into())]);
        } else {
            assert!(!result.status.success());
            assert!(
                stderr(&result).contains("not visible"),
                "{}",
                stderr(&result)
            );
        }
    }
    let direct = ws.run(&["build", "//owner:data"]);
    assert_eq!(
        contents(&direct),
        [("//owner:data".into(), "exported".into())]
    );
    assert!(!ws.0.join("bazel-rs-out").exists());
}

#[test]
fn implicit_sources_use_package_default_not_the_mentioning_rules_visibility() {
    let ws = Workspace::new("filegroup(name='use',srcs=['//owner:data'])\n");
    ws.file("owner/data", "implicit");
    ws.file(
        "owner/BUILD.bazel",
        "filegroup(name='mention',srcs=['data'],visibility=['//visibility:public'])\n",
    );
    let private = ws.run(&["build", ":use"]);
    assert!(!private.status.success());
    assert!(stderr(&private).contains("not visible"));
    ws.file("owner/BUILD.bazel", "package(default_visibility=['//visibility:public'])\nfilegroup(name='mention',srcs=['data'],visibility=['//visibility:private'])\n");
    let public = ws.run(&["build", ":use"]);
    assert_eq!(contents(&public), [("//:use".into(), "implicit".into())]);
    ws.file("owner/BUILD.bazel", "package(default_visibility=['//visibility:public'])\nexports_files(['data'],visibility=['//visibility:private'])\nfilegroup(name='mention',srcs=['data'])\n");
    let explicit_private = ws.run(&["build", ":use"]);
    assert!(!explicit_private.status.success());
    assert!(stderr(&explicit_private).contains("not visible"));
}
