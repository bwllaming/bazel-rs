//! N47: Starlark transitions on the `//command_line_option`s rules_go 0.63.0
//! reads or writes (`platforms`, `compilation_mode`, `collect_code_coverage`)
//! and rules_go's genuine transitions (the unmodified `transition.bzl` of
//! tests/fixtures/starlark_transitions/go_shaped) below probe rules that
//! report the configuration they see (docs/CONFIGURATION.md "Coverage-enabled
//! configurations (N47)"). The expected reports are the native output,
//! equal to genuine Bazel 9.3.0's after D1 and ST-hash normalization
//! (tests/fixtures/go_transitions/reference_receipt.json). Also: Args
//! `map_each` with a top-level function another package's `.bzl` defines
//! (rules_go's toolchain-provided `go.toolchain.actions.stdlib`) and
//! `File.root`.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(module: &str, files: &[(String, String)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("n47-")
                .tempdir_in(base)
                .unwrap(),
        );
        ws.file("MODULE.bazel", module);
        for (path, content) in files {
            ws.file(path, content);
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, path: &str, content: &str) {
        let path = self.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "180s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    /// `LABEL PATH` and the indented contents of every output of a
    /// successful `build ARGS`, in label then path order.
    fn report(&self, args: &[&str]) -> String {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        let output = self.run(&command);
        assert!(output.status.success(), "{args:?}: {}", errors(&output));
        let mut rows = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let (label, path) = line.split_once('\t').unwrap();
            let relative = path.split_once("/outputs/").map_or(path, |(_, rel)| rel);
            let content = fs::read_to_string(path).unwrap();
            rows.push((label.to_owned(), relative.to_owned(), content));
        }
        rows.sort();
        let mut text = String::new();
        for (label, path, content) in rows {
            text.push_str(&format!("{label} {path}\n"));
            for line in content.lines() {
                text.push_str(&format!("  {line}\n"));
            }
        }
        text
    }

    fn rejected(&self, args: &[&str]) -> String {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        let output = self.run(&command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            errors(&output)
        );
        errors(&output).replace(&self.path().display().to_string(), "WS")
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn collect(base: &Path, dir: &Path, prefix: &str, files: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(base, &path, prefix, files);
        } else {
            files.push((
                format!(
                    "{prefix}{}",
                    path.strip_prefix(base).unwrap().to_string_lossy()
                ),
                fs::read_to_string(&path).unwrap(),
            ));
        }
    }
}

/// Replace `-ST-<12 hex>` with `-ST-#` (native hashes cover the modelled
/// options only; the paired capture compares the values separately).
fn normalize(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(index) = rest.find("-ST-") {
        out.push_str(&rest[..index + 4]);
        out.push('#');
        rest = &rest[index + 4 + 12..];
    }
    out.push_str(rest);
    out
}

/// The paired-capture workspace: go_shaped's `go/` tree (genuine
/// transition.bzl) and the probe overlay, as artifacts/go_transitions-capture.py
/// composes it.
fn fixture() -> Workspace {
    let mut files = Vec::new();
    let shaped = manifest().join("tests/fixtures/starlark_transitions/go_shaped/go");
    collect(&shaped, &shaped, "go/", &mut files);
    let overlay = manifest().join("tests/fixtures/go_transitions/workspace");
    collect(&overlay, &overlay, "", &mut files);
    Workspace::new(
        &format!(
            "module(name = \"go_transitions\", repo_name = \"io_bazel_rules_go\")\n\nbazel_dep(name = \"bazel_skylib\", version = \"1.8.2\")\nlocal_path_override(module_name = \"bazel_skylib\", path = \"{}\")\n",
            manifest()
                .join("tests/fixtures/build_settings/skylib")
                .display()
        ),
        &files,
    )
}

fn profile() -> Vec<String> {
    vec![
        "--experimental-builtin-modules".to_owned(),
        format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        ),
        "--host_platform=//p:linux".to_owned(),
    ]
}

const SUCCESS: &[&str] = &[
    "stdlib",
    "tool",
    "non_go_tool",
    "non_go",
    "non_request_nogo",
    "gobin_race",
    "gobin_pure_static",
    "gobin_cross",
    "gobin_cgo",
    "cross_dbg",
    "cross_noop",
    "cov_true",
    "cov_false",
    "cov_str_true",
    "cov_str_yes",
    "cov_str_zero",
    "cov_toggle",
    "cov_then_stdlib",
    "cov_then_tool",
    "cov_twice",
    "opt_str",
    "opt_plat_str",
    "opt_plat_list",
    "opt_same",
    "p",
    "inst_plain",
    "gen_plain",
];

const FLAGS: &[&str] = &[
    "--//go/config:race",
    "--//go/config:tags=netgo,b",
    "--//go/config:linkmode=pie",
    "--//go/config:static",
    "--//go/config:debug",
    "-c",
    "dbg",
];

fn args<'a>(profile: &'a [String], flags: &[&'a str], targets: &'a [String]) -> Vec<&'a str> {
    profile
        .iter()
        .map(String::as_str)
        .chain(flags.iter().copied())
        .chain(targets.iter().map(String::as_str))
        .collect()
}

/// Every success target of the paired capture (S1 default, S2 command-line
/// flags): output roots, `ctx.configuration.coverage_enabled`, `ctx.var`,
/// the target platform, the rules_go settings and the exec tool's root.
#[test]
fn rules_go_transitions_and_command_line_options_follow_bazel() {
    let ws = fixture();
    let profile = profile();
    let targets = SUCCESS
        .iter()
        .map(|name| format!("//probe:{name}"))
        .collect::<Vec<_>>();
    for (flags, expected) in [
        (&[][..], "expected_default.txt"),
        (FLAGS, "expected_flags.txt"),
    ] {
        let actual = normalize(&ws.report(&args(&profile, flags, &targets)));
        let expected = fs::read_to_string(
            manifest()
                .join("tests/fixtures/go_transitions")
                .join(expected),
        )
        .unwrap();
        assert_eq!(actual, expected, "{flags:?}");
    }
    // The configurations themselves (not only their reports): writing the
    // current value is no change; `True` in its bool and string forms is
    // one configuration; the exec transition does not propagate coverage.
    let report = ws.report(&args(&profile, &[], &targets));
    let root = |label: &str| {
        report
            .lines()
            .find(|line| line.starts_with(&format!("{label} ")))
            .map(|line| line.split(' ').nth(1).unwrap().to_owned())
            .unwrap()
    };
    assert_eq!(root("//probe:cov_true"), root("//probe:cov_str_true"));
    assert_eq!(root("//probe:cov_true"), root("//probe:cov_str_yes"));
    assert_eq!(root("//probe:cov_true"), root("//probe:cov_toggle"));
    assert_eq!(root("//probe:cov_false"), root("//probe:cov_str_zero"));
    assert_ne!(root("//probe:cov_true"), root("//probe:cov_false"));
    assert_eq!(
        root("//probe:p"),
        "bazel-out/k8-fastbuild/bin/probe/p.t.txt"
    );
}

/// Invalid `collect_code_coverage` and `compilation_mode` values: Bazel's
/// FunctionTransitionUtil texts (paired capture E/*).
#[test]
fn invalid_option_values_fail_like_bazel() {
    let ws = fixture();
    let profile = profile();
    for (target, message) in [
        (
            "cov_none",
            "IllegalArgumentError for option 'collect_code_coverage': Can not set boolean field com.google.devtools.build.lib.analysis.config.CoreOptions.collectCodeCoverage to null value",
        ),
        (
            "cov_int",
            "Invalid value type for option 'collect_code_coverage'",
        ),
        (
            "cov_bad",
            "OptionsParsingError for option 'collect_code_coverage': 'maybe' is not a boolean",
        ),
        (
            "opt_bad",
            "OptionsParsingError for option 'compilation_mode': Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)",
        ),
    ] {
        let label = [format!("//probe:{target}")];
        let stderr = ws.rejected(&args(&profile, &[], &label));
        assert!(stderr.contains(&format!(": {message}\n")), "{stderr}");
        assert!(
            stderr.contains("Errors encountered while applying Starlark transition"),
            "{stderr}"
        );
    }
}

/// Coverage instrumentation in a coverage-enabled configuration fails
/// explicitly (paired capture G/*: genuine Bazel answers from
/// `--instrumentation_filter` and CoverageOptions, which are not modelled).
#[test]
fn coverage_instrumentation_is_an_explicit_gap() {
    let ws = fixture();
    let profile = profile();
    for (target, what) in [
        ("cov_inst", "ctx.coverage_instrumented()"),
        ("cov_gen", "ctx.fragments.coverage.output_generator"),
    ] {
        let label = [format!("//probe:{target}")];
        let stderr = ws.rejected(&args(&profile, &[], &label));
        assert!(
            stderr.contains(&format!("{what} in a configuration with --collect_code_coverage (set by a Starlark transition) is not implemented in the native profile")),
            "{stderr}"
        );
    }
    // Outside a coverage configuration both answer Bazel's defaults.
    let targets = [
        "//probe:inst_plain".to_owned(),
        "//probe:gen_plain".to_owned(),
    ];
    let report = ws.report(&args(&profile, &[], &targets));
    assert!(report.contains("  instrumented=False\n"), "{report}");
    assert!(report.contains("  output_generator=None\n"), "{report}");
    // A test rule and a late-bound coverage default below a coverage
    // transition fail before their dependencies.
    ws.file(
        "cov/defs.bzl",
        r#"load("//probe:defs.bzl", "cov_edge")
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(out, "x", is_executable = True)
    return [DefaultInfo(executable = out)]
t_test = rule(_impl, test = True)
late = rule(_impl, executable = True, attrs = {"_gen": attr.label(default = configuration_field(fragment = "coverage", name = "output_generator"))})
def _ifi(ctx):
    return [coverage_common.instrumented_files_info(ctx, source_attributes = ["srcs"])]
ifi = rule(_ifi, attrs = {"srcs": attr.label_list(allow_files = True)})
"#,
    );
    ws.file(
        "cov/BUILD.bazel",
        r#"load("//probe:defs.bzl", "cov_edge")
load(":defs.bzl", "ifi", "late", "t_test")
t_test(name = "t_test")
late(name = "late")
ifi(name = "ifi")
cov_edge(name = "cov_ifi", dep = ":ifi", value = "@bool_true")
cov_edge(name = "cov_test", dep = ":t_test", value = "@bool_true", testonly = True)
cov_edge(name = "cov_late", dep = ":late", value = "@bool_true")
"#,
    );
    let stderr = ws.rejected(&args(&profile, &[], &["//cov:cov_test".to_owned()]));
    assert!(
        stderr.contains(
            "analysis of test rule //cov:t_test in a configuration with --collect_code_coverage"
        ),
        "{stderr}"
    );
    let stderr = ws.rejected(&args(&profile, &[], &["//cov:cov_late".to_owned()]));
    assert!(
        stderr.contains("//cov:late: the late-bound attribute '_gen' (configuration_field(\"coverage\", ...)) in a configuration with --collect_code_coverage"),
        "{stderr}"
    );
    // N50's coverage_common.instrumented_files_info is InstrumentedFilesInfo.EMPTY
    // only outside a coverage configuration (integration-15).
    let stderr = ws.rejected(&args(&profile, &[], &["//cov:cov_ifi".to_owned()]));
    assert!(
        stderr.contains("coverage_common.instrumented_files_info in a configuration with --collect_code_coverage (set by a Starlark transition) is not implemented in the native profile"),
        "{stderr}"
    );
    ws.report(&args(&profile, &[], &["//cov:ifi".to_owned()]));
    // Without coverage both analyze (the test rule's implicit test-runner
    // edges, N23g, reach rules_shell's sh_binary, analyzable since N50).
    ws.report(&args(
        &profile,
        &[],
        &["//cov:t_test".to_owned(), "//cov:late".to_owned()],
    ));
}

/// Args.validateMapEach accepts any global function (Args.java:366-385):
/// also one a dependency's package defined and passed in a provider, as
/// rules_go's toolchain passes `go.toolchain.actions.stdlib`; closures stay
/// rejected. `File.root` is ArtifactRoot (FileRootApi): `path`, repr,
/// hashable.
#[test]
fn map_each_from_another_package_and_file_root() {
    let ws = fixture();
    let profile = profile();
    let report = ws.report(&args(&profile, &[], &["//use:u".to_owned()]));
    assert_eq!(
        report,
        "//use:u bazel-out/k8-fastbuild/bin/use/u.roots\n  [\"\", \"bazel-out/k8-fastbuild/bin\", \"\"] [<source root>, <derived root>, <source root>] 2 root [\"path\"] True True\n//use:u bazel-out/k8-fastbuild/bin/use/u.txt\n  use bazel-out/k8-fastbuild/bin/use\n"
    );
    // An external-repository source file's root has the empty exec path
    // like the main repository's (ArtifactRoot.asExternalSourceRoot,
    // ArtifactRoot.java:115-123); built-in callables as map_each.
    let report = ws.report(&args(&profile, &[], &["//use:ext".to_owned()]));
    assert_eq!(
        report,
        "//use:ext bazel-out/k8-fastbuild/bin/use/ext.roots\n  [\"\", \"bazel-out/k8-fastbuild/bin\", \"\"] [<source root>, <derived root>, <source root>] 2 root [\"path\"] True False\n//use:ext bazel-out/k8-fastbuild/bin/use/ext.txt\n  external/bazel_skylib+ bazel-out/k8-fastbuild/bin/use <source file LICENSE> <generated file use/ext.gen> \"LICENSE\"\n"
    );
    let stderr = ws.rejected(&args(&profile, &[], &["//use:c".to_owned()]));
    assert!(
        stderr.contains("map_each must be a top-level function"),
        "{stderr}"
    );
}

/// Raw string-list transition values (SettingValue::Sequence) everywhere a
/// setting value is consumed: `config_setting(flag_values)` matches a list
/// element, a list (or tuple) equal to the default written through an alias
/// is removed (the top-level configuration again), a tuple is a different
/// configuration than an equal list, and `ctx.build_setting_value` keeps the
/// tuple.
#[test]
fn raw_string_list_values_in_selects_aliases_and_identities() {
    let ws = fixture();
    let profile = profile();
    let targets =
        ["list", "tuple", "default", "default_tuple", "s"].map(|name| format!("//raw:{name}"));
    let report = normalize(&ws.report(&args(&profile, &[], &targets)));
    assert_eq!(
        report,
        "//raw:default bazel-out/k8-fastbuild/bin/raw/s.txt\n  bazel-out/k8-fastbuild/bin list [] other\n\
//raw:default_tuple bazel-out/k8-fastbuild/bin/raw/s.txt\n  bazel-out/k8-fastbuild/bin list [] other\n\
//raw:list bazel-out/k8-fastbuild-ST-#/bin/raw/s.txt\n  bazel-out/k8-fastbuild-ST-#/bin list [\"netgo\", \"x\"] netgo\n\
//raw:s bazel-out/k8-fastbuild/bin/raw/s.txt\n  bazel-out/k8-fastbuild/bin list [] other\n\
//raw:tuple bazel-out/k8-fastbuild-ST-#/bin/raw/s.txt\n  bazel-out/k8-fastbuild-ST-#/bin tuple (\"netgo\", \"x\") netgo\n"
    );
    let raw = ws.report(&args(&profile, &[], &targets));
    let root = |label: &str| {
        raw.lines()
            .find(|line| line.starts_with(&format!("{label} ")))
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .to_owned()
    };
    assert_ne!(root("//raw:list"), root("//raw:tuple"));
}
