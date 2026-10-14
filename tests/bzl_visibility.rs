//! Real native CLI contracts for Bazel 9.3 `.bzl` load visibility
//! (BazelBuildApiGlobals.visibility, BzlLoadFunction.checkLoadVisibilities).
//! All fixtures are offline/local; no genuine Bazel is executed here.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bzl_visibility");

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("bzl-visibility-cli-")
                .tempdir()
                .unwrap(),
        );
        for directory in ["home", "tmp"] {
            fs::create_dir(f.0.path().join(directory)).unwrap();
        }
        copy_tree(
            &Path::new(FIXTURE).join("package_metadata"),
            &f.0.path().join("package_metadata"),
        );
        f.file("MODULE.bazel", "module(name='main')\nbazel_dep(name='package_metadata',version='0.0.5')\nlocal_path_override(module_name='package_metadata',path='package_metadata')\n");
        f.file("BUILD.bazel", "");
        f.file("data.txt", "data\n");
        f
    }

    fn file(&self, name: &str, contents: &str) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("/usr/bin/timeout")
            .args(["--kill-after=5s", "60s"])
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .args(["--ignore_all_rc_files", "--cache-dir=cache"])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            !matches!(output.status.code(), None | Some(124 | 137)),
            "{}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains("panicked"), "{}", stderr(&output));
        output
    }

    fn query(&self, target: &str) -> String {
        let output = self.run(&["query", target]);
        assert!(output.status.success(), "{}", stderr(&output));
        String::from_utf8(output.stdout).unwrap()
    }

    /// The workspace path as Bazel prints it in event locations.
    fn ws(&self) -> String {
        fs::canonicalize(self.0.path())
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn reject(&self, target: &str) -> String {
        let output = self.run(&["query", target]);
        assert!(!output.status.success(), "{}", stderr(&output));
        assert!(output.stdout.is_empty());
        stderr(&output)
    }
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target: PathBuf = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn violation(file: &str, package: &str) -> String {
    format!(
        "Starlark file {file} is not visible for loading from package {package}. Check the file's `visibility()` declaration."
    )
}

#[test]
fn public_external_provider_file_loads_from_build_and_bzl() {
    let f = Fixture::new();
    f.file("defs.bzl", "load('@package_metadata//providers:package_metadata_info.bzl', 'PackageMetadataInfo', 'package_metadata_purl')\nvisibility('public')\ndef _impl(ctx):\n    return [DefaultInfo(), PackageMetadataInfo(metadata=None, purl='pkg:x')]\nmetadata_rule = rule(implementation=_impl)\n");
    f.file("pkg/BUILD.bazel", "load('@package_metadata//providers:package_metadata_info.bzl', 'PackageMetadataInfo')\nload('//:defs.bzl', 'metadata_rule')\nmetadata_rule(name='m')\n");
    assert_eq!(f.query("//pkg:m"), "//pkg:m\n");
    let output = f.run(&["build", "//pkg:m"]);
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn private_external_file_rejected_from_main_build_with_exact_text() {
    let f = Fixture::new();
    f.file(
        "BUILD.bazel",
        "load('@package_metadata//providers:internal.bzl', 'INTERNAL')\nfilegroup(name='g')\n",
    );
    let error = f.reject("//:g");
    assert!(
        error.contains(&format!(
            "{}/BUILD.bazel:1:6: {}",
            f.ws(),
            violation("@@package_metadata+//providers:internal.bzl", "//")
        )),
        "{error}"
    );
    assert!(
        error.contains("file //:BUILD.bazel contains .bzl load visibility violations"),
        "{error}"
    );
}

#[test]
fn private_and_listed_visibility_between_main_packages() {
    let f = Fixture::new();
    f.file(
        "a/BUILD.bazel",
        "load(':private.bzl', 'P')\nfilegroup(name='same')\n",
    );
    f.file("a/private.bzl", "visibility('private')\nP = 1\n");
    f.file(
        "a/listed.bzl",
        "visibility(['//b/...', '//c:__pkg__'])\nL = 1\n",
    );
    f.file("a/default.bzl", "D = 1\n");
    // Same package is always allowed, even for private files.
    assert_eq!(f.query("//a:same"), "//a:same\n");
    // Listed packages (with subpackages) and the default public visibility.
    for package in ["b", "b/sub", "c"] {
        f.file(
            &format!("{package}/BUILD.bazel"),
            "load('//a:listed.bzl', 'L')\nload('//a:default.bzl', 'D')\nfilegroup(name='ok')\n",
        );
        assert_eq!(
            f.query(&format!("//{package}:ok")),
            format!("//{package}:ok\n")
        );
    }
    // Every violating load statement is reported, then the summary line.
    f.file(
        "c/sub/BUILD.bazel",
        "load('//a:listed.bzl', 'L')\nload('//a:private.bzl', 'P')\nfilegroup(name='bad')\n",
    );
    let error = f.reject("//c/sub:bad");
    let first = format!(
        "{}/c/sub/BUILD.bazel:1:6: {}",
        f.ws(),
        violation("//a:listed.bzl", "//c/sub")
    );
    let second = format!(
        "{}/c/sub/BUILD.bazel:2:6: {}",
        f.ws(),
        violation("//a:private.bzl", "//c/sub")
    );
    let summary = "file //c/sub:BUILD.bazel contains .bzl load visibility violations";
    for line in [&first, &second, summary] {
        assert!(error.contains(line), "{error}");
    }
    assert!(error.find(&first) < error.find(&second));
    assert!(error.find(&second) < error.find(summary));
}

#[test]
fn bzl_to_bzl_violation_names_the_requesting_module() {
    let f = Fixture::new();
    f.file("a/BUILD.bazel", "");
    f.file("a/private.bzl", "visibility('private')\nP = 1\n");
    f.file(
        "b/BUILD.bazel",
        "load(':user.bzl', 'U')\nfilegroup(name='g')\n",
    );
    f.file("b/user.bzl", "load('//a:private.bzl', 'P')\nU = P\n");
    let error = f.reject("//b:g");
    assert!(
        error.contains(&format!(
            "{}/b/user.bzl:1:6: {}",
            f.ws(),
            violation("//a:private.bzl", "//b")
        )),
        "{error}"
    );
    assert!(
        error.contains("module //b:user.bzl contains .bzl load visibility violations"),
        "{error}"
    );
}

#[test]
fn module_extension_bzl_with_visibility_runs_and_checks_transitive_loads() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\nbazel_dep(name='package_metadata',version='0.0.5')\nlocal_path_override(module_name='package_metadata',path='package_metadata')\ne=use_extension('//ext:ext.bzl','files')\nuse_repo(e,'made')\n");
    f.file(
        "BUILD.bazel",
        "genrule(name='copy',srcs=['@made//:data.txt'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\n",
    );
    f.file("ext/BUILD.bazel", "");
    f.file("ext/internal/BUILD.bazel", "");
    f.file(
        "ext/internal/helper.bzl",
        "visibility(['//ext'])\nTEXT = 'made by helper'\n",
    );
    f.file("ext/rules.bzl", "visibility('public')\ndef _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt', ctx.attr.text)\nmaker = repository_rule(_impl, attrs={'text': attr.string()})\n");
    // The entry file is private to //ext, yet MODULE.bazel may still use it:
    // RegularRunnableExtension does not check the use_extension load.
    f.file("ext/ext.bzl", "load('@package_metadata//providers:package_metadata_info.bzl', 'PackageMetadataInfo')\nload('//ext/internal:helper.bzl', 'TEXT')\nload(':rules.bzl', 'maker')\nvisibility('private')\ndef _impl(ctx):\n    maker(name='made', text=TEXT)\nfiles = module_extension(_impl)\n");
    let output = f.run(&["build", "//:copy"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let (_, path) = stdout.lines().next().unwrap().split_once('\t').unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "made by helper");

    // A transitive load from the extension file is checked like any .bzl load.
    f.file(
        "ext/internal/helper.bzl",
        "visibility('private')\nTEXT = 'made by helper'\n",
    );
    let error = f.reject("@made//:data.txt");
    assert!(
        error.contains(&format!(
            "{}/ext/ext.bzl:2:6: {}",
            f.ws(),
            violation("//ext/internal:helper.bzl", "//ext")
        )),
        "{error}"
    );
    assert!(
        error.contains("module //ext:ext.bzl contains .bzl load visibility violations"),
        "{error}"
    );
}

#[test]
fn visibility_call_errors_through_cli() {
    let f = Fixture::new();
    f.file(
        "a/BUILD.bazel",
        "load(':defs.bzl', 'X')\nfilegroup(name='g')\n",
    );
    for (source, message) in [
        (
            "def f():\n    visibility('public')\nf()\nX = 1\n",
            "load visibility may only be set at the top level, not inside a function",
        ),
        (
            "visibility('public')\nvisibility('public')\nX = 1\n",
            "load visibility may not be set more than once",
        ),
        (
            "visibility(('//a',))\nX = 1\n",
            "Invalid visibility: got 'tuple', want string or list of strings",
        ),
        (
            "visibility(['//a', 3])\nX = 1\n",
            "at index 1 of visibility list, got element of type int, want string",
        ),
        (
            "visibility('-//a')\nX = 1\n",
            "Cannot use negative package patterns here",
        ),
        (
            "visibility('a')\nX = 1\n",
            "invalid package name 'a': must start with '//', '@', or be 'public' or 'private'",
        ),
        (
            "visibility('//b:foo')\nX = 1\n",
            "invalid package name '//b:foo': invalid target name 'foo:__pkg__': target names may not contain ':'",
        ),
        (
            "visibility('//b/../c')\nX = 1\n",
            "invalid package name '//b/../c': invalid package name 'b/../c': package name component contains only '.' characters",
        ),
        (
            "visibility('@package_metadata')\nX = 1\n",
            "invalid package name '@package_metadata': invalid repository name 'package_metadata:__pkg__': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and '+'",
        ),
        (
            "def f():\n    visibility('public')\nX = f\n",
            "visibility() can only be used during .bzl initialization (top-level evaluation)",
        ),
    ] {
        let source = if message.starts_with("visibility()") {
            // Called later, from a BUILD file, after initialization finished.
            f.file("a/BUILD.bazel", "load(':defs.bzl', 'X')\nX()\n");
            source
        } else {
            source
        };
        f.file("a/defs.bzl", source);
        let error = f.reject("//a:g");
        assert!(error.contains(message), "{message}: {error}");
    }
    // `visibility` is a .bzl-only global; BUILD files do not predeclare it.
    f.file("a/BUILD.bazel", "visibility('public')\n");
    let error = f.reject("//a:g");
    assert!(error.contains("visibility"), "{error}");
    assert!(!error.contains("load visibility"), "{error}");
}
