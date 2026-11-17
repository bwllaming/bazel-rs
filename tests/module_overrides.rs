//! MODULE overrides of nonregistry and extension repositories
//! (docs/MODULE_OVERRIDES.md): `override_repo` / `inject_repo`,
//! `archive_override` and `git_override`. Offline: a local file:// archive
//! and a local Git remote, the explicit-module profile. Expected values are
//! genuine Bazel 9.3.1's observations of the same fixtures in
//! tests/fixtures/module_overrides/reference_receipt.json
//! (tests/fixtures/module_overrides/capture.py).
#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
include!("support/console.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/module_overrides")
}

fn receipt() -> serde_json::Value {
    serde_json::from_slice(&fs::read(fixtures().join("reference_receipt.json")).unwrap()).unwrap()
}

struct Fixture {
    temp: tempfile::TempDir,
    ws: PathBuf,
}

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

impl Fixture {
    fn new(name: &str) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("module-overrides-")
            .tempdir_in(crate::test_temp::root())
            .unwrap();
        for directory in ["home", "tmp"] {
            fs::create_dir(temp.path().join(directory)).unwrap();
        }
        let ws = temp.path().join("ws");
        copy_tree(&fixtures().join(name), &ws);
        Self { temp, ws }
    }

    fn template(&self, replacements: &[(&str, String)], drop_attribute: Option<&str>) {
        let mut text = fs::read_to_string(self.ws.join("MODULE.bazel.in")).unwrap();
        for (from, to) in replacements {
            text = text.replace(from, to);
        }
        if let Some(attribute) = drop_attribute {
            text = text
                .lines()
                .filter(|line| !line.trim().starts_with(&format!("{attribute} =")))
                .map(|line| format!("{line}\n"))
                .collect();
        }
        fs::write(self.ws.join("MODULE.bazel"), text).unwrap();
    }

    fn append_module(&self, text: &str) {
        let path = self.ws.join("MODULE.bazel");
        let mut module = fs::read_to_string(&path).unwrap();
        module.push_str(text);
        fs::write(path, module).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    fn run_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let output = Command::new("/usr/bin/timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.temp.path().join("cache").display()
            ))
            .args(args)
            .current_dir(&self.ws)
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.temp.path().join("home"))
            .env("TMPDIR", self.temp.path().join("tmp"))
            .env("LC_ALL", "C")
            .env("BAZEL_RS_PROGRESS", "0")
            .envs(envs.iter().copied())
            .explicit_modules()
            .output()
            .unwrap();
        assert!(!matches!(output.status.code(), Some(124 | 137)), "timeout");
        assert!(!stderr(&output).contains("panicked"), "{}", stderr(&output));
        output
    }

    fn ws_text(&self) -> String {
        self.ws.display().to_string()
    }

    /// The `"moduleExtensions"` section of MODULE.bazel.lock, verbatim
    /// (the explicit-module profile writes no lockfile without any).
    fn lockfile_extensions(&self) -> String {
        let Ok(text) = fs::read_to_string(self.ws.join("MODULE.bazel.lock")) else {
            return "  \"moduleExtensions\": {},\n".to_owned();
        };
        let start = text.find("  \"moduleExtensions\": ").unwrap();
        let end = text.find("  \"facts\": ").unwrap();
        text[start..end].to_owned()
    }

    fn output_file(&self, name: &str) -> String {
        let output = self.run(&["info", "bazel-bin"]);
        let bin = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        fs::read_to_string(Path::new(&bin).join(name)).unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn debug_lines(output: &Output, ws: &str) -> Vec<String> {
    stderr(output)
        .lines()
        .filter(|line| line.starts_with("DEBUG: "))
        .map(|line| line.replace(ws, "$WS"))
        .collect()
}

#[test]
fn override_repo_and_inject_repo_redirect_extension_repositories() {
    let expected = &receipt()["repo_override"];
    let f = Fixture::new("repo_override");
    let output = f.run(&["build", "//:g", "@gen_b//:x"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let mut prints: Vec<String> = without_print_locations(&stderr(&output))
        .lines()
        .filter(|line| line.starts_with("OVR|"))
        .map(str::to_owned)
        .collect();
    prints.sort();
    prints.dedup();
    let expected_prints: Vec<String> = expected["prints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(prints, expected_prints);
    // The usages digest covers the overrides (canonical names, in Java
    // HashMap order): byte-identical with genuine's lockfile entry.
    assert_eq!(
        f.lockfile_extensions(),
        expected["lockfile_extensions"].as_str().unwrap()
    );
    let output = f.run(&["mod", "dump_repo_mapping", "", "+ext+gen_b"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mappings: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (actual, expected) in mappings
        .iter()
        .zip([&expected["root_mapping"], &expected["gen_b_mapping"]])
    {
        let expected = expected.as_object().unwrap();
        let actual = actual.as_object().unwrap();
        assert_eq!(
            actual.iter().collect::<std::collections::BTreeMap<_, _>>(),
            expected
                .iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        );
    }
}

#[test]
fn overridden_generated_repositories_rebind_from_their_lockfile_entry() {
    // A reproducible extension is evaluated in every command; its result
    // is re-probed against the entry the next command would read, which
    // binds each generated repository under its own canonical name even
    // where the mapping carries an override (`gen_a`, `mine`).
    let f = Fixture::new("repo_override");
    let ext = fs::read_to_string(f.ws.join("ext.bzl")).unwrap().replace(
        "    plain(name = \"gen_d\", tag = \"unused\")\n",
        "    plain(name = \"gen_d\", tag = \"unused\")\n    return mctx.extension_metadata(reproducible = True)\n",
    );
    fs::write(f.ws.join("ext.bzl"), ext).unwrap();
    let output = f.run_env(&["build", "//:g"], &[("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("bazel-rs-snapshot: "), "{text}");
    assert!(!text.contains("not re-probed"), "{text}");
    assert!(!text.contains("extension-evaluated"), "{text}");
}

#[test]
fn repo_override_misuse_fails_with_bazel_messages() {
    let variants = [
        (
            "override_not_generated",
            "override_repo(ext, nothere = \"dep\")\n",
        ),
        ("inject_generated", "inject_repo(ext, gen_d = \"dep\")\n"),
        ("invisible", "override_repo(ext, gen_c = \"depp\")\n"),
        ("import_injected", "inject_repo(ext, gen_b = \"dep\")\n"),
        (
            "duplicate",
            "override_repo(ext, gen_c = \"dep\")\noverride_repo(ext, gen_c = \"dep2\")\n",
        ),
        (
            "override_of_override",
            "override_repo(ext, gen_b = \"gen_a\")\n",
        ),
        ("bad_proxy", "override_repo(\"x\", gen_b = \"dep\")\n"),
    ];
    let receipt = receipt();
    for (name, extra) in variants {
        let f = Fixture::new("repo_override");
        f.append_module(extra);
        let output = f.run(&["build", "--nobuild", "//:g"]);
        assert!(!output.status.success(), "{name}");
        let actual = stderr(&output).replace(&f.ws_text(), "$WS");
        // The message genuine reports, without its traceback framing: the
        // `Error in FUNCTION: ` line of a MODULE failure, the text after
        // `build aborted: ` of an extension's.
        let errors = receipt["repo_override"]["variants"][name]["errors"]
            .as_array()
            .unwrap();
        let message = errors
            .iter()
            .filter_map(|line| {
                let line = line.as_str().unwrap();
                line.split_once("Error in ")
                    .filter(|(lead, _)| lead.is_empty())
                    .and_then(|(_, rest)| rest.split_once(": "))
                    .map(|(_, message)| message)
                    .or_else(|| line.split_once("build aborted: ").map(|(_, m)| m))
            })
            .next()
            .unwrap_or_else(|| panic!("{name}: no message in the receipt"));
        assert!(
            actual.contains(message),
            "{name}: expected {message:?} in\n{actual}"
        );
    }
}

#[test]
fn archive_override_fetches_patches_and_runs_patch_commands() {
    let receipt = receipt();
    for (name, dropped) in [
        ("archive", None),
        ("archive_no_integrity", Some("integrity")),
    ] {
        let expected = &receipt[name];
        let f = Fixture::new("archive");
        let url = format!("file://{}", f.ws.join("arch.tar.gz").display());
        f.template(&[("@ARCHIVE_URL@", url)], dropped);
        let output = f.run(&["build", "//:g"]);
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        assert_eq!(
            debug_lines(&output, &f.ws_text()),
            expected["debug"]
                .as_array()
                .unwrap()
                .iter()
                .map(|line| line.as_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            "{name}"
        );
        assert_eq!(f.output_file("g.txt"), expected["output"].as_str().unwrap());
        assert_eq!(
            f.lockfile_extensions(),
            expected["lockfile_extensions"].as_str().unwrap()
        );
        // A second command reuses the fetched tree: nothing is printed.
        let output = f.run(&["build", "//:g"]);
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        assert!(debug_lines(&output, &f.ws_text()).is_empty(), "{name}");
    }
}

#[test]
fn archive_override_integrity_mismatch_fails() {
    let f = Fixture::new("archive");
    let url = format!("file://{}", f.ws.join("arch.tar.gz").display());
    f.template(&[("@ARCHIVE_URL@", url)], None);
    let module = fs::read_to_string(f.ws.join("MODULE.bazel"))
        .unwrap()
        .replace("sha256-ev+", "sha256-ew+");
    fs::write(f.ws.join("MODULE.bazel"), module).unwrap();
    let output = f.run(&["build", "--nobuild", "//:g"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("integrity mismatch"),
        "{}",
        stderr(&output)
    );
}

const GIT_ENV: &[(&str, &str)] = &[
    ("PATH", "/usr/bin:/bin"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_AUTHOR_NAME", "fixture"),
    ("GIT_AUTHOR_EMAIL", "fixture@example.invalid"),
    ("GIT_COMMITTER_NAME", "fixture"),
    ("GIT_COMMITTER_EMAIL", "fixture@example.invalid"),
    ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
    ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
];

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("HOME", cwd)
        .envs(GIT_ENV.iter().copied())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// capture.py's make_git_remote.
fn make_git_remote(path: &Path) {
    fs::create_dir_all(path.join("sub")).unwrap();
    for (relative, text) in [
        (
            "sub/MODULE.bazel",
            "module(name = \"gitmod\", version = \"3.0\")\n",
        ),
        (
            "sub/BUILD.bazel",
            "filegroup(name = \"all\", srcs = glob([\"*.txt\"]), visibility = [\"//visibility:public\"])\n",
        ),
        ("sub/data.txt", "from git\n"),
        ("top.txt", "outside the prefix\n"),
    ] {
        fs::write(path.join(relative), text).unwrap();
    }
    git(path, &["init", "--quiet", "-b", "main"]);
    git(path, &["add", "."]);
    let gitlink = format!("160000,{},sub/submod", "1".repeat(40));
    git(path, &["update-index", "--add", "--cacheinfo", &gitlink]);
    git(path, &["commit", "--quiet", "-m", "fixture"]);
    git(path, &["tag", "-a", "v1", "-m", "v1"]);
}

#[test]
fn git_override_tag_with_submodule_gitlink_strip_prefix_and_patches() {
    let expected = &receipt()["git"];
    let f = Fixture::new("git");
    let remote = f.temp.path().join("git-remote");
    make_git_remote(&remote);
    let commit = git(&remote, &["rev-parse", "v1^{commit}"])
        .trim()
        .to_owned();
    f.template(
        &[("@REMOTE@", format!("file://{}", remote.display()))],
        None,
    );
    let output = f.run(&["build", "//:g"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let debug: Vec<String> = debug_lines(&output, &f.ws_text())
        .into_iter()
        .map(|line| line.replace(&commit, "$COMMIT"))
        .collect();
    assert_eq!(
        debug,
        expected["debug"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(f.output_file("g.txt"), expected["output"].as_str().unwrap());
    // The tag stays pinned to the commit it named when first fetched.
    fs::write(remote.join("sub/data.txt"), "moved\n").unwrap();
    git(&remote, &["commit", "--quiet", "-am", "moved"]);
    git(&remote, &["tag", "-f", "-a", "v1", "-m", "v1 again"]);
    let output = f.run(&["build", "//:g"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(debug_lines(&output, &f.ws_text()).is_empty());
    assert_eq!(f.output_file("g.txt"), expected["output"].as_str().unwrap());
}

/// The branch's commit as `build //:g` sees it: the output lists the
/// remote's extra file only from the second commit on.
fn built_with_extra(f: &Fixture) -> bool {
    let output = f.run(&["build", "//:g"]);
    assert!(output.status.success(), "{}", stderr(&output));
    f.output_file("g.txt").contains("extra")
}

#[test]
fn git_override_branch_pin_is_per_workspace_like_a_marker_file() {
    let f = Fixture::new("git");
    let remote = f.temp.path().join("git-remote");
    make_git_remote(&remote);
    f.template(
        &[("@REMOTE@", format!("file://{}", remote.display()))],
        None,
    );
    let module = fs::read_to_string(f.ws.join("MODULE.bazel"))
        .unwrap()
        .replace("tag = \"v1\"", "branch = \"main\"");
    fs::write(f.ws.join("MODULE.bazel"), &module).unwrap();
    assert!(!built_with_extra(&f));
    // The branch moves.
    fs::write(remote.join("sub/extra.txt"), "extra\n").unwrap();
    git(&remote, &["add", "sub/extra.txt"]);
    git(&remote, &["commit", "--quiet", "-m", "extra"]);
    // The same workspace keeps its pin (Bazel's marker file)...
    assert!(!built_with_extra(&f));
    // ...a fresh workspace sharing the cache resolves again...
    let other = Fixture {
        temp: tempfile::Builder::new()
            .prefix("module-overrides-")
            .tempdir_in(crate::test_temp::root())
            .unwrap(),
        ws: PathBuf::new(),
    };
    let other = Fixture {
        ws: other.temp.path().join("ws"),
        ..other
    };
    for directory in ["home", "tmp"] {
        fs::create_dir(other.temp.path().join(directory)).unwrap();
    }
    copy_tree(&f.ws, &other.ws);
    let _ = fs::remove_file(other.ws.join("MODULE.bazel.lock"));
    let shared = |fixture: &Fixture, args: &[&str]| {
        let mut all = vec![format!(
            "--cache-dir={}",
            f.temp.path().join("cache").display()
        )];
        all.extend(args.iter().map(|arg| (*arg).to_owned()));
        let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .args(&all)
            .current_dir(&fixture.ws)
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", fixture.temp.path().join("home"))
            .env("TMPDIR", fixture.temp.path().join("tmp"))
            .env("BAZEL_RS_PROGRESS", "0")
            .explicit_modules()
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        output
    };
    shared(&other, &["build", "//:g"]);
    let bin = String::from_utf8(shared(&other, &["info", "bazel-bin"]).stdout).unwrap();
    assert!(
        fs::read_to_string(Path::new(bin.trim()).join("g.txt"))
            .unwrap()
            .contains("extra")
    );
    // ...and so do `clean --expunge` and `fetch --force` in this one.
    let output = f.run(&["clean", "--expunge"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(built_with_extra(&f));
    git(&remote, &["rm", "--quiet", "sub/extra.txt"]);
    git(&remote, &["commit", "--quiet", "-m", "no extra"]);
    assert!(built_with_extra(&f));
    let output = f.run(&["fetch", "--force", "--repo=@gitmod", EXPLICIT_MODULES]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("and dropping [branch]"),
        "{}",
        stderr(&output)
    );
    assert!(!built_with_extra(&f));
}

#[test]
fn fixture_is_shared_with_the_paired_capture() {
    for path in [
        "capture.py",
        "repo_override/MODULE.bazel",
        "repo_override/dep_user/MODULE.bazel",
        "archive/MODULE.bazel.in",
        "archive/arch.tar.gz",
        "git/MODULE.bazel.in",
    ] {
        assert!(fixtures().join(path).is_file(), "{path}");
    }
}
