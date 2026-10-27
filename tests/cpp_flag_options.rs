//! Starlark transitions on the CppOptions flag lists, `--crosstool_top`
//! and the exec transition's `--host_*` mapping (N66), captured with
//! genuine Bazel 9.3 (tests/fixtures/cpp_flag_options/reference.json).
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
struct Fixture(tempfile::TempDir);

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy(&e.path(), &dst)
        } else {
            fs::copy(e.path(), dst).unwrap();
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let f = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        copy(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/cpp_flag_options/workspace"),
            f.path(),
        );
        for d in ["home", "tmp"] {
            fs::create_dir(f.path().join(d)).unwrap();
        }
        f
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn cmd(&self, target: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        c.current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_RESULT_LINES", "1")
            .env("BAZEL_RS_CACHE_GC", "off")
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--platforms=//:p",
                "--host_platform=//:p",
                "--memory-limit=2048",
            ])
            .arg(target);
        c
    }
    fn output(&self, target: &str) -> Output {
        self.cmd(target).output().unwrap()
    }
    fn built(&self, target: &str) -> Vec<(String, PathBuf)> {
        let o = self.output(target);
        assert!(
            o.status.success(),
            "{target}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout)
            .unwrap()
            .lines()
            .filter_map(|s| s.split_once('\t'))
            .map(|(l, p)| (l.to_owned(), PathBuf::from(p)))
            .collect()
    }
    fn error(&self, target: &str) -> String {
        let o = self.output(target);
        assert!(!o.status.success(), "{target} unexpectedly succeeded");
        String::from_utf8_lossy(&o.stderr).to_string()
    }
}

#[test]
fn cpp_flag_transitions_match_genuine() {
    let f = Fixture::new();
    // reference.json "outputs": every path and content, exactly.
    let flags = f.built("//:flags");
    let path = flags[0].1.to_string_lossy().to_string();
    assert!(
        path.contains("bazel-out/k8-fastbuild-ST-34436af9073b/bin/flags.out"),
        "{path}"
    );
    assert_eq!(
        fs::read_to_string(&flags[0].1).unwrap(),
        "([\"-O2\", \"-DNDEBUG\"], [\"-std=c++17\"], [\"-std=c11\"], [\"-lm\"])"
    );
    // The default lists do not enter the output path.
    let plain = f.built("//:plain");
    let plain_path = plain[0].1.to_string_lossy().to_string();
    assert!(
        plain_path.contains("bazel-out/k8-fastbuild/bin/plain.out"),
        "{plain_path}"
    );
    assert_eq!(fs::read_to_string(&plain[0].1).unwrap(), "([], [], [], [])");
    // per_file_copt: the reader children of //:per_file (its own config and
    // the top-level default), path and content; the ST- hash uses the
    // normalized union pattern, the read the original spelling. The writer
    // targets forward their children's files.
    for (target, out, dir, content) in [
        (
            "//:per_file",
            "per_file_after.out",
            "bazel-out/k8-fastbuild-ST-6e15fd9a1933",
            "[\"//pkg/.*\\\\.cc,-//pkg/skip\\\\.cc@-O0,-pg\", \"//other/@-g3\"]",
        ),
        (
            "//:per_file_after",
            "per_file_after.out",
            "bazel-out/k8-fastbuild-ST-350d3f8b752c",
            "[]",
        ),
        (
            "//:host",
            "host_after.out",
            "bazel-out/k8-fastbuild-ST-2965e904c542",
            "([\"-Os\"], [\"-fno-rtti\"])",
        ),
        (
            "//:host_after",
            "host_after.out",
            "bazel-out/k8-fastbuild-ST-1d470c2a02e3",
            "([], [])",
        ),
        (
            "//:crosstool",
            "crosstool_after.out",
            "bazel-out/k8-fastbuild-ST-c7b9dc8a69eb",
            "@@//:flags",
        ),
        (
            "//:crosstool_after",
            "crosstool_after.out",
            "bazel-out/k8-fastbuild-ST-eae889f02454",
            "@@bazel_tools//tools/cpp:toolchain",
        ),
    ] {
        let built = f.built(target);
        let path = built
            .first()
            .unwrap_or_else(|| panic!("{target}: no result line"))
            .1
            .to_string_lossy()
            .to_string();
        assert!(
            path.contains(&format!("{dir}/bin/{out}")),
            "{target}: {path} not under {dir}"
        );
        assert_eq!(
            fs::read_to_string(PathBuf::from(&path)).unwrap_or_default(),
            content,
            "{target} content"
        );
    }
}

#[test]
fn exec_transition_maps_host_flags_with_g0() {
    let f = Fixture::new();
    // reference.json "outputs": the exec dependencies' output paths.
    for (target, tool_dir) in [
        ("//:exec_plain", "bazel-out/p-opt-exec-ST-56c600ff889b"),
        ("//:exec_host", "bazel-out/p-opt-exec-ST-88aca3559b90"),
    ] {
        let built = f.built(target);
        let content = fs::read_to_string(&built[0].1).unwrap();
        assert_eq!(
            content,
            format!("\"{tool_dir}/bin/plain.out\""),
            "{target} tool path"
        );
    }
}

#[test]
fn transition_errors_match_genuine() {
    let f = Fixture::new();
    let errors = [
        (
            "//:bad_regex",
            "OptionsParsingError for option 'per_file_copt': Failed to build valid regular expression:",
        ),
        (
            "//:none_list",
            "'None' value not allowed for List-type option 'copt'. Please use '[]' instead if trying to set option to empty value.",
        ),
        (
            "//:string_form",
            "IllegalArgumentError for option 'copt': Can not set java.util.List field com.google.devtools.build.lib.rules.cpp.CppOptions.coptList to java.lang.String",
        ),
    ];
    for (target, expected) in errors {
        let error = f.error(target);
        assert!(error.contains(expected), "{target}: {error}");
    }
}
