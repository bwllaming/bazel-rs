//! L2-W4, the plan/execute split (PLAN-L2 §1d, src/request.rs): every
//! build, test and run goes plan -> PlannedRequest -> finalize -> execute ->
//! report, and under `BAZEL_RS_SNAPSHOT_ROUNDTRIP=1` it executes the request
//! after a round trip through the binary codec (src/snapshot/codec.rs).
//!
//! * The round trip changes nothing: exit code, stdout and stderr (with
//!   the `BAZEL_RS_DUMP_ACTIONS` dump) are byte-identical with and without
//!   it, for `build`, `build --nobuild`, `test`, `run` (of a binary and of
//!   a test), cache hits and a failing workspace status command — on a
//!   fixture with tree artifacts expanded by Args (deferred command lines
//!   and written Args), workspace status consumers, genrules (an embedded
//!   input) and inherited environment variables.
//! * The determinism gate (PLAN-L2 §1b layer 4): the encoded request
//!   (`BAZEL_RS_SNAPSHOT_PLAN_OUT`) is identical across
//!   `--loading_phase_threads` 1/8/32 and loading-order jitter seeds.
//! * The record mode decides the build-info taint on the plan: a
//!   translation callback analysis registered but the plan pruned leaves
//!   the run eligible; one the plan keeps blocks it.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DEFS: &str = r##"def _tree_impl(ctx):
    d = ctx.actions.declare_directory(ctx.label.name + "_dir")
    ctx.actions.run_shell(
        outputs = [d],
        command = "mkdir -p {d}/sub && echo a > {d}/a.txt && echo b > {d}/sub/b.txt".format(d = d.path),
    )
    listed = ctx.actions.declare_file(ctx.label.name + ".list")
    args = ctx.actions.args()
    args.add_all([d], format_each = "f=%s")
    args.add("--x")
    args.use_param_file("@%s", use_always = True)
    ctx.actions.run_shell(
        outputs = [listed],
        inputs = [d] + ctx.files.deps,
        arguments = [args],
        command = "cat ${1#@} > " + listed.path,
        use_default_shell_env = True,
    )
    written = ctx.actions.declare_file(ctx.label.name + ".params")
    wargs = ctx.actions.args()
    wargs.add_all([d])
    ctx.actions.write(written, wargs)
    status = ctx.actions.declare_file(ctx.label.name + ".status")
    ctx.actions.run_shell(
        outputs = [status],
        inputs = [ctx.info_file, ctx.version_file],
        command = "grep -c BUILD {} {} > {}".format(ctx.info_file.path, ctx.version_file.path, status.path),
    )
    env = ctx.actions.declare_file(ctx.label.name + ".env")
    ctx.actions.run_shell(
        outputs = [env],
        command = "echo \"$W4_INHERITED\" > " + env.path,
        use_default_shell_env = True,
    )
    return [DefaultInfo(files = depset([listed, written, status, env]))]

tree = rule(implementation = _tree_impl, attrs = {"deps": attr.label_list(allow_files = True)})

def _exe(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [
        DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data)),
        RunEnvironmentInfo(environment = ctx.attr.env),
    ]

_ATTRS = {
    "script": attr.string(),
    "data": attr.label_list(allow_files = True),
    "env": attr.string_dict(),
}

script_test = rule(implementation = _exe, test = True, attrs = _ATTRS)
script_binary = rule(implementation = _exe, executable = True, attrs = _ATTRS)
"##;

const ROOT_BUILD: &str = r##"load("//rules:defs.bzl", "script_binary", "script_test", "tree")

tree(name = "t", deps = ["//p00:g00", "//p05:g05"])

genrule(name = "g", outs = ["g.txt"], cmd = "echo hi > $@")

script_test(
    name = "pass_test",
    size = "small",
    data = [":g"],
    env = {"FOO": "bar"},
    script = "#!/bin/bash\necho FOO=$FOO\ncat g.txt\n",
)

script_test(name = "fail_test", script = "#!/bin/bash\necho failing\nexit 1\n")

script_binary(name = "tool", data = [":g"], script = "#!/bin/bash\necho tool-ran \"$@\"\ncat g.txt\n")
"##;

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("planned-request-")
                .tempdir_in(base)
                .unwrap(),
        );
        for (path, content) in files {
            let path = ws.0.path().join("ws").join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        fs::create_dir_all(ws.0.path().join("home")).unwrap();
        ws
    }

    /// The fixture: the root package and twelve small packages (parallel
    /// loading has something to reorder).
    fn fixture() -> Self {
        let mut files: Vec<(String, String)> = vec![
            ("MODULE.bazel".into(), "module(name = \"planned\")\n".into()),
            ("BUILD.bazel".into(), ROOT_BUILD.into()),
            ("rules/BUILD.bazel".into(), String::new()),
            ("rules/defs.bzl".into(), DEFS.into()),
        ];
        for index in 0..12 {
            files.push((
                format!("p{index:02}/BUILD.bazel"),
                format!(
                    "genrule(name = 'g{index:02}', srcs = ['in.txt'], outs = ['out{index:02}.txt'], cmd = 'cp $(location in.txt) $@', visibility = ['//visibility:public'])\n"
                ),
            ));
            files.push((format!("p{index:02}/in.txt"), format!("package {index}\n")));
        }
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_str()))
            .collect();
        Self::new(&borrowed)
    }

    fn root(&self) -> PathBuf {
        self.0.path().join("ws")
    }

    fn cache(&self) -> PathBuf {
        self.0.path().join("cache")
    }

    /// A fresh cache at the same path (runfiles actions key absolute
    /// paths): outputs are read-only, so make them writable first.
    fn wipe_cache(&self) {
        fn writable(path: &Path) {
            if let Ok(metadata) = fs::symlink_metadata(path) {
                if metadata.is_dir() {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
                    for entry in fs::read_dir(path).unwrap().flatten() {
                        writable(&entry.path());
                    }
                } else if !metadata.file_type().is_symlink() {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
                }
            }
        }
        let cache = self.cache();
        writable(&cache);
        let _ = fs::remove_dir_all(&cache);
    }

    fn run(&self, arguments: &[&str], env: &[(&str, &str)]) -> Output {
        let home = self.0.path().join("home");
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "180s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.cache().display()))
            .args(arguments)
            .current_dir(self.root())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("TMPDIR", &home)
            .env("LC_ALL", "C")
            // Inherited by `--action_env=W4_INHERITED` (resolved by finalize).
            .env("W4_INHERITED", "from-client")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }
}

/// What a run printed, for comparison: exit code, stdout, stderr, with
/// test durations (`PASSED in 0.1s`) normalized.
fn observed(output: &Output) -> (Option<i32>, String, String) {
    (
        output.status.code(),
        without_durations(&String::from_utf8_lossy(&output.stdout)),
        without_durations(&String::from_utf8_lossy(&output.stderr)),
    )
}

/// ` in <digits>.<digits>s` -> ` in <T>s`.
fn without_durations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(" in ") {
        out.push_str(&rest[..at + 4]);
        rest = &rest[at + 4..];
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if digits > 0 && rest[..digits].contains('.') && rest[digits..].starts_with('s') {
            out.push_str("<T>");
            rest = &rest[digits..];
        }
    }
    out.push_str(rest);
    out
}

/// Every command shape executes the decoded request exactly like the
/// planned one.
#[test]
fn round_tripped_requests_execute_byte_identically() {
    let ws = Workspace::fixture();
    let cases: &[(&[&str], bool)] = &[
        (&["build", "//:t", "//:g"], false),
        (&["build", "//:t", "//:g"], true),
        (&["build", "--action_env=W4_INHERITED", "//:t"], false),
        (&["build", "--nobuild", "//:t", "//:g"], false),
        (&["test", "//:all"], false),
        (&["test", "--nobuild", "//:pass_test"], false),
        (&["run", "//:tool", "--", "a", "b"], false),
        (&["run", "//:pass_test"], false),
        (&["run", "--nobuild", "//:tool"], false),
        (
            &["build", "--workspace_status_command=/bin/false", "//:t"],
            false,
        ),
    ];
    for (arguments, warm) in cases {
        let mut runs = Vec::new();
        for roundtrip in [false, true] {
            ws.wipe_cache();
            let mut env = vec![("BAZEL_RS_DUMP_ACTIONS", "1")];
            if roundtrip {
                env.push(("BAZEL_RS_SNAPSHOT_ROUNDTRIP", "1"));
            }
            if *warm {
                ws.run(arguments, &env);
            }
            runs.push(observed(&ws.run(arguments, &env)));
        }
        assert_eq!(runs[0], runs[1], "{arguments:?} (warm: {warm})");
        let (code, _, stderr) = &runs[0];
        assert!(stderr.contains("ACTION "), "{arguments:?}: {stderr}");
        match arguments[0] {
            "build"
                if arguments
                    .iter()
                    .any(|a| a.starts_with("--workspace_status")) =>
            {
                assert_eq!(*code, Some(1), "{stderr}");
                assert!(
                    stderr.contains("Failed to determine workspace status"),
                    "{stderr}"
                );
            }
            "test" if arguments.contains(&"//:all") => assert_eq!(*code, Some(3), "{stderr}"),
            _ if arguments.contains(&"--nobuild") && arguments[0] != "build" => {
                assert_eq!(*code, Some(1), "{stderr}")
            }
            _ => assert_eq!(*code, Some(0), "{arguments:?}: {stderr}"),
        }
    }
    // The deferred expansions and the status really ran (decoded request).
    ws.wipe_cache();
    let built = ws.run(
        &["build", "--action_env=W4_INHERITED", "//:t"],
        &[("BAZEL_RS_SNAPSHOT_ROUNDTRIP", "1")],
    );
    assert!(built.status.success(), "{}", observed(&built).2);
    let stdout = String::from_utf8_lossy(&built.stdout);
    let output = |suffix: &str| {
        let path = stdout
            .lines()
            .find_map(|line| {
                line.split_once('\t')
                    .map(|(_, path)| path)
                    .filter(|p| p.ends_with(suffix))
            })
            .unwrap_or_else(|| panic!("{suffix} in {stdout}"));
        fs::read_to_string(path).unwrap()
    };
    assert_eq!(
        output("t.list"),
        "'f=bazel-out/k8-fastbuild/bin/t_dir/a.txt'\n'f=bazel-out/k8-fastbuild/bin/t_dir/sub/b.txt'\n--x\n"
    );
    assert_eq!(
        output("t.params"),
        "bazel-out/k8-fastbuild/bin/t_dir/a.txt\nbazel-out/k8-fastbuild/bin/t_dir/sub/b.txt\n"
    );
    assert_eq!(output("t.env"), "from-client\n");
}

/// PLAN-L2 §1b layer 4: the encoded request does not depend on the
/// loading order or the thread count (no hash order, no per-process ids, no
/// workspace status bytes: the status nodes are placeholders).
#[test]
fn encoded_requests_are_identical_across_threads_and_loading_jitter() {
    let ws = Workspace::fixture();
    let plans = ws.0.path().join("plans");
    fs::create_dir_all(&plans).unwrap();
    for arguments in [
        &["build", "//..."][..],
        &["build", "--nobuild", "//..."][..],
        &["test", "//:pass_test"][..],
    ] {
        let mut reference: Option<Vec<u8>> = None;
        for (threads, jitter) in [(1usize, 0u64), (8, 0), (32, 0), (8, 7), (32, 40), (8, 99)] {
            let file = plans.join(format!("{}-{threads}-{jitter}.plan", arguments.len()));
            let file_text = file.display().to_string();
            let threads_flag = format!("--loading_phase_threads={threads}");
            let jitter_text = jitter.to_string();
            let mut env = vec![("BAZEL_RS_SNAPSHOT_PLAN_OUT", file_text.as_str())];
            if jitter > 0 {
                env.push(("BAZEL_RS_LOADING_JITTER", jitter_text.as_str()));
            }
            let mut command = vec![arguments[0], threads_flag.as_str()];
            command.extend(&arguments[1..]);
            let output = ws.run(&command, &env);
            assert!(
                output.status.success() || arguments[0] == "test",
                "{command:?}: {}",
                observed(&output).2
            );
            let bytes = fs::read(&file).unwrap();
            assert!(bytes.starts_with(b"BZRSPLN1"));
            match &reference {
                None => reference = Some(bytes),
                Some(reference) => assert!(
                    *reference == bytes,
                    "{command:?} with jitter {jitter}: the encoded request differs"
                ),
            }
        }
    }
}

const BUILD_INFO_DEFS: &str = r#"def _group_impl(ctx):
    return [DefaultInfo(files = getattr(ctx.attr.translator[OutputGroupInfo], ctx.attr.group))]

build_info_group = rule(
    implementation = _group_impl,
    attrs = {
        "group": attr.string(),
        "translator": attr.label(providers = [OutputGroupInfo]),
    },
)
"#;

const BUILD_INFO_BUILD: &str = r#"load(":defs.bzl", "build_info_group")

build_info_group(
    name = "redacted",
    group = "redacted_build_info_files",
    translator = "@bazel_tools//tools/build_defs/build_info:cc_build_info",
)

build_info_group(
    name = "stamped",
    group = "non_redacted_build_info_files",
    translator = "@bazel_tools//tools/build_defs/build_info:cc_build_info",
)
"#;

fn snapshot_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix("bazel-rs-snapshot: "))
        .map(str::to_owned)
        .collect()
}

/// PLAN-L2 update 1 §(c)4: the build-info taint is decided on the planned
/// request. bazel_tools' cc_build_info registers its stamped
/// transform_version_file/transform_info_file actions whenever it is
/// analyzed; a build that only needs the redacted group prunes them and
/// stays eligible, one that builds the stamped headers is not.
#[test]
fn record_mode_decides_build_info_eligibility_on_the_plan() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name = \"planned_build_info\")\n"),
        ("BUILD.bazel", ""),
        ("bi/defs.bzl", BUILD_INFO_DEFS),
        ("bi/BUILD.bazel", BUILD_INFO_BUILD),
    ]);
    let registry = format!(
        "--registry=file://{}/tests/fixtures/builtin_modules/registry",
        manifest().display()
    );
    let build = |target: &str| {
        ws.run(
            &["build", "--experimental-builtin-modules", &registry, target],
            &[("BAZEL_RS_SNAPSHOT", "record")],
        )
    };
    // The first run publishes the embedded bazel_tools (not eligible).
    let first = build("//bi:redacted");
    assert!(first.status.success(), "{}", observed(&first).2);
    let warm = build("//bi:redacted");
    assert!(warm.status.success(), "{}", observed(&warm).2);
    let lines = snapshot_lines(&warm);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("record: command=build ")
                && line.ends_with(" eligible=yes")),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line
            == "plan check: build-info-callback registered 2 (2 distinct), 0 in the plan; map-each-deferred registered 0 (0 distinct), 0 in the plan"),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line == "taints: none"),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("plan: 3 actions, encoded ")),
        "{lines:#?}"
    );
    let stamped = build("//bi:stamped");
    assert!(stamped.status.success(), "{}", observed(&stamped).2);
    let lines = snapshot_lines(&stamped);
    assert!(
        lines.iter().any(|line| line.ends_with(" eligible=no")),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line.starts_with(
            "taint build-info-callback (in the planned request): 2 occurrence(s), 2 distinct: @@bazel_tools//tools/build_defs/build_info:cc_build_info#"
        )),
        "{lines:#?}"
    );
}
