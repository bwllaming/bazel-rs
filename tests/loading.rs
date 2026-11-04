#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

include!("support/snapshot_off.rs");
include!("support/console.rs");
include!("support/explicit_modules.rs");
include!("support/sealed_trees.rs");

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let path = crate::test_temp::root().join(format!(
            "bazel-rs-loading-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='loading_fixture')\n");
        ws.file("BUILD.bazel", "");
        ws
    }
    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }
    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .args(args)
            .envs(env.iter().copied())
            .current_dir(&self.0)
            .explicit_modules()
            .output()
            .unwrap()
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = sealed_trees::remove_tree(&self.0);
    }
}
fn stderr(o: &Output) -> String {
    let stderr = without_print_locations(&String::from_utf8_lossy(&o.stderr));
    // Startup notice, not a diagnostic (crate::memory_limit).
    let stderr = stderr
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
        .collect::<Vec<_>>()
        .join("\n");
    if stderr.is_empty() {
        stderr
    } else {
        stderr + "\n"
    }
}
fn outputs(o: &Output) -> Vec<PathBuf> {
    assert!(o.status.success(), "{}", stderr(o));
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| PathBuf::from(l.split_once('\t').unwrap().1))
        .collect()
}

#[test]
fn frozen_macros_use_callers_package_and_native_is_a_struct() {
    let ws = Workspace::new();
    ws.file("macros/BUILD.bazel", "");
    ws.file(
        "macros/helper.bzl",
        "CONFIG = struct(suffix='.txt')\ndef output_name(name):\n    return name + CONFIG.suffix\n",
    );
    ws.file("macros/nested/defs.bzl", "load(':helper.bzl', rename='output_name')\ndef copy(name, *, prefix=''):\n    if type(native) != 'struct':\n        fail('native must be struct')\n    native.genrule(name=name,srcs=native.glob(['*.in']),outs=[rename(name)],cmd='cat $(SRCS) > $@')\n    native.filegroup(name=name+'_context', srcs=[], visibility=['//visibility:public'])\n");
    ws.file(
        "BUILD.bazel",
        "load('//macros:nested/defs.bzl','copy')\ncopy(name='root')\n",
    );
    ws.file("input.in", "root context\n");
    ws.file(
        "other/BUILD.bazel",
        "load('//macros:nested/defs.bzl','copy')\ncopy(name='other')\n",
    );
    ws.file("other/input.in", "other context\n");
    let result = ws.run(&["build", "--jobs=2", "//:root", "//other:other"]);
    let paths = outputs(&result);
    assert_eq!(paths.len(), 2);
    assert_eq!(fs::read_to_string(&paths[0]).unwrap(), "root context\n");
    assert_eq!(fs::read_to_string(&paths[1]).unwrap(), "other context\n");
    assert!(stderr(&result).contains("2 actions executed"));
}

#[test]
fn package_defaults_apply_to_earlier_rules_but_not_exports() {
    let ws = Workspace::new();
    ws.file("pkg/BUILD.bazel", "filegroup(name='implicit',srcs=['data.txt'])\nfilegroup(name='private',srcs=['data.txt'],visibility=['//visibility:private'])\npackage(default_visibility=['//visibility:public'])\n");
    ws.file("pkg/data.txt", "defaulted");
    ws.file("private/BUILD.bazel", "package(default_visibility=['//visibility:private'])\nexports_files(['exported.txt'],visibility=None,licenses=None)\nexports_files(['exported.txt'])\n");
    ws.file("private/exported.txt", "public export");
    ws.file("BUILD.bazel", "filegroup(name='use',srcs=['//pkg:implicit','//pkg:data.txt','//private:exported.txt'])\nfilegroup(name='bad',srcs=['//pkg:private'])\n");
    assert_eq!(outputs(&ws.run(&["build", ":use"])).len(), 2);
    let result = ws.run(&["build", ":bad"]);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("not visible"));
}

#[test]
fn glob_excludes_subpackages_and_handles_dotfiles_like_bazel() {
    let ws = Workspace::new();
    ws.file("BUILD.bazel", "filegroup(name='files',srcs=glob(['**/*.txt'],exclude=['skip.txt']))\nfilegroup(name='hidden',srcs=glob(['.*.txt']))\n");
    for path in [
        "root.txt",
        "skip.txt",
        ".hidden.txt",
        "sub/deep.txt",
        "nested/subpackage.txt",
    ] {
        ws.file(path, path);
    }
    ws.file("nested/BUILD.bazel", "");
    let paths = outputs(&ws.run(&["build", ":files"]));
    let relative: Vec<_> = paths
        .iter()
        .map(|p| p.strip_prefix(&ws.0).unwrap())
        .collect();
    assert_eq!(relative, [Path::new("root.txt"), Path::new("sub/deep.txt")]);
    assert_eq!(
        outputs(&ws.run(&["build", ":hidden"])),
        [ws.0.join(".hidden.txt")]
    );
}

#[test]
fn empty_globs_are_errors_by_default_and_before_any_execution() {
    let ws = Workspace::new();
    for src in [
        "glob(['missing'])",
        "glob(['present','missing'])",
        "glob(['present'],exclude=['present'])",
        "glob(['present'],exclude=None)",
    ] {
        ws.file("present", "exists");
        ws.file("BUILD.bazel", &format!("genrule(name='first',outs=['out'],cmd='echo hi > $@')\nfilegroup(name='bad',srcs={src})\n"));
        // No cache directory after the failed loading: the analysis
        // snapshot (on by default) would set up its store there first.
        let result = ws.run_with(&["build", ":first"], &[SNAPSHOT_OFF]);
        assert!(!result.status.success(), "accepted {src}");
        assert!(!ws.0.join("bazel-rs-out").exists());
    }
    ws.file(
        "BUILD.bazel",
        "filegroup(name='empty',srcs=glob(['missing'],allow_empty=True))\n",
    );
    assert!(ws.run(&["build", ":empty"]).status.success());
}

#[test]
fn bzl_top_level_package_mutation_and_load_cycles_fail() {
    let ws = Workspace::new();
    ws.file("BUILD.bazel", "load(':bad.bzl','value')\n");
    for source in [
        "native.filegroup(name='injected')\nvalue=1",
        "value=native.glob(['*'])",
        "native.exports_files(['x'])\nvalue=1",
    ] {
        ws.file("bad.bzl", source);
        let result = ws.run(&["query", "//:all"]);
        assert!(!result.status.success());
        assert!(stderr(&result).contains("outside package evaluation"));
        assert!(result.stdout.is_empty());
    }
    ws.file("bad.bzl", "load(':other.bzl','value')\n");
    ws.file("other.bzl", "load(':bad.bzl','value')\n");
    assert!(stderr(&ws.run(&["query", "//:all"])).contains("load cycle"));
}

// Bazel evaluates each .bzl once per invocation (BzlLoadValue): a top-level
// print appears once however many packages load the file, while macro prints
// run per call. Load visibility is still checked for every loading package.
#[test]
fn bzl_modules_evaluate_once_per_invocation_and_check_visibility_per_package() {
    let ws = Workspace::new();
    let cache = ws.0.join("cache");
    let cache = format!("--cache-dir={}", cache.display());
    ws.file("defs/BUILD.bazel", "");
    ws.file("defs/base.bzl", "print('base top-level')\nbase = 'f.txt'\n");
    ws.file(
        "defs/defs.bzl",
        "load(':base.bzl', 'base')\nprint('defs top-level')\ndef m(name):\n    print('macro ' + name)\n    native.filegroup(name = name, srcs = [base])\n",
    );
    for package in ["a", "b", "c"] {
        ws.file(
            &format!("{package}/BUILD.bazel"),
            &format!("load('//defs:defs.bzl', 'm')\nm(name = '{package}')\n"),
        );
    }
    let result = ws.run(&[&cache, "query", "//..."]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert_eq!(
        stderr(&result),
        "base top-level\ndefs top-level\nmacro a\nmacro b\nmacro c\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&result.stdout),
        "//a:a\n//b:b\n//c:c\n"
    );
    ws.file(
        "defs/defs.bzl",
        "visibility(['//a/...', '//c/...'])\nprint('defs top-level')\ndef m(name):\n    native.filegroup(name = name)\n",
    );
    let result = ws.run(&[&cache, "query", "//..."]);
    assert!(!result.status.success());
    assert_eq!(
        stderr(&result),
        format!(
            "defs top-level\nbazel-rs: {}:1:6: Starlark file //defs:defs.bzl is not visible for loading from package //b. Check the file's `visibility()` declaration.\nfile //b:BUILD.bazel contains .bzl load visibility violations\n",
            ws.0.join("b/BUILD.bazel").display()
        )
    );
}

// The N44 memos and indexes are hash maps/sets (per-process random seeds):
// none may be iterated into output. Two processes over a many-package,
// many-input workspace must print byte-identical query and build output and
// key the same actions.
#[test]
fn repeated_processes_produce_byte_identical_outputs() {
    let ws = Workspace::new();
    ws.file("defs/BUILD.bazel", "");
    ws.file(
        "defs/helpers.bzl",
        "print('helpers loaded')\ndef suffix(name):\n    return name + '.out'\n",
    );
    ws.file(
        "defs/defs.bzl",
        r#"load(":helpers.bzl", "suffix")
Info = provider(fields = ["files"])
def _impl(ctx):
    out = ctx.actions.declare_file(suffix(ctx.label.name))
    args = ctx.actions.args()
    args.add_all(ctx.files.srcs)
    ctx.actions.run_shell(
        outputs = [out],
        inputs = ctx.files.srcs + ctx.files.deps,
        arguments = [args],
        command = "cat \"$@\" > " + out.path,
    )
    return [DefaultInfo(files = depset([out])), Info(files = depset([out]))]
collect = rule(
    implementation = _impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "deps": attr.label_list(allow_files = True),
    },
)
def bundle(name, srcs):
    print("bundle " + native.package_name() + ":" + name)
    collect(name = name + "_c", srcs = srcs)
    native.genrule(name = name + "_g", srcs = srcs, outs = [name + "_g.txt"], cmd = "cat $(SRCS) | sort > $@")
    native.filegroup(name = name, srcs = [":" + name + "_c", ":" + name + "_g"])
"#,
    );
    for p in 0..24 {
        for f in 0..12 {
            ws.file(
                &format!("p{p:02}/d{}/f{f:02}.txt", f % 3),
                &format!("{p} {f}\n"),
            );
        }
        let dep = if p == 0 {
            String::new()
        } else {
            format!("'//p{:02}:b0', ", p - 1)
        };
        ws.file(
            &format!("p{p:02}/BUILD.bazel"),
            &format!(
                "load('//defs:defs.bzl', 'bundle', 'collect')\npackage(default_visibility = ['//visibility:public'])\nbundle(name = 'b0', srcs = glob(['**/*.txt']))\nbundle(name = 'b1', srcs = glob(['d1/*.txt', 'd2/*.txt'], exclude = ['d2/f05.txt']))\ncollect(name = 'all', srcs = glob(['**/*.txt']), deps = [{dep}':b0', ':b1'])\nexports_files(glob(['d0/*.txt']))\n"
            ),
        );
    }
    let run = |index: usize, args: &[&str]| {
        let cache = ws.0.join(format!("cache{index}"));
        let mut argv = vec![format!("--cache-dir={}", cache.display())];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        let result = ws.run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(result.status.success(), "{}", stderr(&result));
        let normalize = |bytes: &[u8]| {
            without_elapsed(&String::from_utf8_lossy(bytes))
                .replace(&cache.display().to_string(), "CACHE")
        };
        // Cache entries' key names; the workspace status actions are
        // excluded (their keys embed the volatile timestamp, which two
        // runs may straddle — they are system actions since the N63
        // follow-up, not the determinism surface under test).
        let mut keys = fs::read_dir(cache.join("actions"))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| {
                        !path.join("outputs/bazel-out/stable-status.txt").exists()
                            && !path.join("outputs/bazel-out/volatile-status.txt").exists()
                    })
                    .map(|path| path.file_name().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        keys.sort();
        (normalize(&result.stdout), normalize(&result.stderr), keys)
    };
    for args in [&["query", "//..."][..], &["build", "--jobs=4", "//..."][..]] {
        let first = run(0, args);
        let second = run(1, args);
        assert_eq!(first, second, "{args:?}");
        assert_eq!(first.1.matches("helpers loaded").count(), 1);
        assert_eq!(first.1.matches("bundle p07:b1").count(), 1);
        if args[0] == "build" {
            assert_eq!(first.2.len(), 24 * 5);
        }
    }
}

#[test]
fn package_exports_and_build_syntax_are_strict() {
    let ws = Workspace::new();
    for source in [
        "package()",
        "package(default_visibility=None)",
        "package(default_visibility=[])\npackage(default_visibility=[])",
        "exports_files(['x'],licenses=['notice'])",
        "exports_files(['x'])\nexports_files(['x'],visibility=['//visibility:private'])",
        "exports_files(['x'])\nfilegroup(name='x')",
        "exports_files(['x'])\ngenrule(name='g',outs=['x'],cmd='true')",
        "filegroup(**{'name':'x'})",
        "filegroup(name='x',srcs=list(*[['x']]))",
        "native.filegroup(name='x')",
        "value=struct(name='x')",
    ] {
        ws.file("BUILD.bazel", source);
        assert!(
            !ws.run(&["query", "//:all"]).status.success(),
            "accepted {source}"
        );
    }
}
