//! N35: exec groups through the CLI (docs/EXEC_GROUPS.md): declared groups,
//! per-group toolchain resolution, `ctx.exec_groups`, automatic exec groups,
//! `ctx.actions.run`/`run_shell(exec_group = ..., toolchain = ...)`,
//! `exec_properties` and `exec_group_compatible_with` per group,
//! `config.exec(exec_group = ...)` edges, subrules under automatic exec
//! groups and the default `test` exec group. Expected texts are genuine
//! Bazel 9.3 strings (paired capture
//! tests/fixtures/exec_groups/reference_receipt.json). The workspace is
//! tests/fixtures/exec_groups/workspace with N30's builtin-closure stubs
//! (tests/fixtures/test_flags/stubs).
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

const STUBBED: &[&str] = &[
    "apple_support",
    "buildozer",
    "protobuf",
    "rules_cc",
    "rules_java",
    "rules_python",
    "rules_shell",
    "zlib",
];

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("exec-groups-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        copy_tree(
            &manifest().join("tests/fixtures/exec_groups/workspace"),
            f.0.path(),
        );
        let stubs = manifest().join("tests/fixtures/test_flags/stubs");
        let mut module = "module(name = 'main')\nregister_toolchains('//tcs:all')\n".to_owned();
        for name in STUBBED {
            module.push_str(&format!(
                "local_path_override(module_name = '{name}', path = '{}')\n",
                stubs.join(name).display()
            ));
        }
        fs::write(f.0.path().join("MODULE.bazel"), module).unwrap();
        f
    }

    fn run_profile(&self, builtin: bool, command: &str, args: &[&str]) -> Output {
        let registry = format!(
            "--registry=file://{}",
            manifest()
                .join("tests/fixtures/builtin_modules/registry")
                .display()
        );
        let mut all = Vec::new();
        if builtin {
            all.extend(["--experimental-builtin-modules", registry.as_str()]);
        }
        all.extend_from_slice(args);
        Command::new("timeout")
            .args([
                "300s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
            ])
            .args(all)
            .current_dir(self.0.path())
            .output()
            .unwrap()
    }

    fn build(&self, args: &[&str]) -> Output {
        self.run_profile(true, "build", args)
    }

    fn path(&self) -> String {
        self.0.path().display().to_string()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn ok(output: &Output) -> String {
    assert!(output.status.success(), "{}", stderr(output));
    stderr(output)
}

fn rejected(output: &Output, message: &str) -> String {
    let text = stderr(output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains(message), "expected {message:?} in:\n{text}");
    text
}

fn has_lines(text: &str, lines: &[&str]) {
    for line in lines {
        assert!(
            text.lines().any(|seen| seen == *line),
            "expected line {line:?} in:\n{text}"
        );
    }
}

const EG_PRINTS: &[&str] = &[
    "EG1 ExecGroupCollection <ctx.exec_groups: empty, link> True True False False []",
    "EG2 ExecGroupContext <exec_group_context> [\"toolchains\"] ToolchainContext <toolchain_context.resolved_labels: //eg:t1, //eg:t2opt>",
    "EG3 t1_impl True False None",
    "EG4 <toolchain_context.resolved_labels: > <toolchain_context.resolved_labels: > [] [Label(\"//eg:t1\"), Label(\"//eg:t2opt\")]",
];

#[test]
fn declared_exec_groups_resolve_per_group() {
    let f = Fixture::new();
    has_lines(&ok(&f.build(&["//eg:g"])), EG_PRINTS);
    for (target, message) in [
        (
            "g_unknown",
            "In groups rule //eg:g_unknown, unrecognized exec group 'zzz' requested. Available exec groups: [empty, link]",
        ),
        (
            "g_int",
            "exec groups only support indexing by exec group name, got 1 of type int instead",
        ),
        (
            "g_int_in",
            "exec groups only support indexing by exec group name, got 1 of type int instead",
        ),
        (
            "g_default",
            "In groups rule //eg:g_default, unrecognized exec group 'default-exec-group' requested. Available exec groups: [empty, link]",
        ),
        (
            "g_not_requested",
            "In groups rule //eg:g_not_requested, toolchain type //eg:t2 was requested but only types [//eg:t1, //eg:t2opt] are configured",
        ),
        (
            "g_default_tc",
            "In groups rule //eg:g_default_tc, toolchain type //eg:t1 was requested but only types [] are configured",
        ),
    ] {
        rejected(&f.build(&[&format!("//eg:{target}")]), message);
    }
    let ws = f.path();
    // Per-group resolution errors, reported like the default context's.
    let text = rejected(
        &f.build(&["//eg:strict"]),
        "Unable to find an execution platform for target platform @@platforms//host:host from available execution platforms []",
    );
    assert!(
        text.contains(&format!(
            "{ws}/eg/BUILD.bazel:39:7: While resolving toolchains for target //eg:strict ("
        )),
        "{text}"
    );
    has_lines(
        &ok(&f.build(&["--host_platform=//eg:hostcv", "//eg:strict"])),
        &["STRICT <ctx.exec_groups: cvg>"],
    );
    rejected(
        &f.build(&["//eg:missing"]),
        "No matching toolchains found for types:\n  //eg:t3\nTo debug, rerun with --toolchain_resolution_debug='//eg:t3'",
    );
    rejected(
        &f.build(&["//eg:noinfo"]),
        "toolchain type //eg:tnoinfo resolved to target //eg:noinfo_impl, but that target does not provide ToolchainInfo",
    );
}

#[test]
fn actions_choose_exec_groups_and_exec_properties_are_per_group() {
    let f = Fixture::new();
    ok(&f.build(&["//eg:g_actions", "//eg:g_shell_tools", "//eg:g_run"]));
    for (target, message) in [
        (
            "g_shell_nope",
            "Action declared for non-existent exec group 'nope'.",
        ),
        (
            "g_shell_default",
            "Invalid name for exec group 'default-exec-group'.",
        ),
        (
            "g_shell_auto_name",
            "Action declared for non-existent exec group '//eg:t1'.",
        ),
        (
            "g_shell_int",
            "in call to run_shell(), parameter 'exec_group' got value of type 'int', want 'string or NoneType'",
        ),
        (
            "g_shell_test",
            "Action declared for non-existent exec group 'test'.",
        ),
        (
            "g_run_toolchain_int",
            "in call to run(), parameter 'toolchain' got value of type 'int', want 'Label, string, or NoneType'",
        ),
        (
            "g_run_toolchain_syntax",
            "invalid target name 'a:b': target names may not contain ':'",
        ),
    ] {
        rejected(&f.build(&[&format!("//eg:{target}")]), message);
    }
    let ws = f.path();
    ok(&f.build(&["//eg:g_props"]));
    ok(&f.build(&["--host_platform=//eg:hostprops", "//eg:g_props"]));
    rejected(
        &f.build(&["//eg:g_props_bad"]),
        &format!(
            "{ws}/eg/BUILD.bazel:32:7: Tried to set exec_properties for non-existent exec groups on //eg:g_props_bad: lnk (did you mean 'link'?)"
        ),
    );
    rejected(
        &f.build(&["//eg:g_props_bad_two"]),
        "Tried to set exec_properties for non-existent exec groups on //eg:g_props_bad_two: zz,lnk (did you mean 'link'?)",
    );
    rejected(
        &f.build(&["//eg:g_props_test"]),
        "Tried to set exec_properties for non-existent exec groups on //eg:g_props_test: test",
    );
    // exec_group_compatible_with adds execution constraints to a group.
    rejected(
        &f.build(&["//eg:g_egcw"]),
        "While resolving toolchains for target //eg:g_egcw (",
    );
    rejected(
        &f.build(&["//eg:g_egcw_empty"]),
        "Unable to find an execution platform for target platform @@platforms//host:host from available execution platforms []",
    );
    ok(&f.build(&[
        "--host_platform=//eg:hostcv",
        "//eg:g_egcw",
        "//eg:g_egcw_empty",
    ]));
    rejected(
        &f.build(&["//eg:g_egcw_nope"]),
        &format!(
            "{ws}/eg/BUILD.bazel:36:7: Tried to set execution constraints for non-existent exec groups on //eg:g_egcw_nope: lnk (did you mean 'link'?)"
        ),
    );
    rejected(
        &f.build(&["//eg:g_egcw_label"]),
        "Tried to set execution constraints for non-existent exec groups on //eg:g_egcw_label: //eg:t1\n",
    );
    // config.exec(exec_group = "link"): the group's execution platform (the
    // host platform, without the target platform's constraint).
    let text = ok(&f.build(&["--platforms=//eg:other", "//eg:g_tools"]));
    has_lines(&text, &["DEP //eg:dep True", "DEP //eg:dep False"]);
    rejected(
        &f.build(&["//eg:g_nope_tool"]),
        "Attr 'nope_tool' declares a transition for non-existent exec group 'nope'",
    );
}

const AUTO_PRINTS: &[&str] = &[
    "AUTO1 <ctx.exec_groups: //eg:t1, //eg:t2opt, link> <toolchain_context.resolved_labels: //eg:t1, //eg:t2opt> t1_impl None True",
    "AUTO2 t1_impl <toolchain_context.resolved_labels: //eg:t1> False [Label(\"//eg:t1\"), Label(\"//eg:t2opt\")]",
];

const TOOLCHAIN_UNSET: &str = "Couldn't identify if tools are from implicit dependencies or a toolchain. Please set the toolchain parameter. If you're not using a toolchain, set it to 'None'.";

#[test]
fn automatic_exec_groups_follow_the_attribute_and_the_flag() {
    let f = Fixture::new();
    has_lines(&ok(&f.build(&["//eg:a"])), AUTO_PRINTS);
    ok(&f.build(&[
        "//eg:a_shell",
        "//eg:a_shell_tools_none",
        "//eg:a_shell_tools_t1",
        "//eg:a_shell_tools_link",
        "//eg:a_run_none",
        "//eg:a_run_t1",
        "//eg:a_run_t1_label",
        "//eg:a_run_t1_relative",
        "//eg:a_run_link_t2opt",
        "//eg:a_run_link",
        "//eg:a_run_string",
    ]));
    for (target, message) in [
        (
            "a_not_requested",
            "In auto rule //eg:a_not_requested, toolchain type //eg:t3 was requested but only types [//eg:t1, //eg:t2opt] are configured",
        ),
        ("a_shell_tools", TOOLCHAIN_UNSET),
        ("a_shell_tools_depset", TOOLCHAIN_UNSET),
        ("a_run", TOOLCHAIN_UNSET),
        (
            "a_run_t3",
            "Action declared for non-existent toolchain '//eg:t3'.",
        ),
        (
            "a_run_link_t1",
            "`toolchain` and `exec_group` parameters inside actions.{run, run_shell} are not compatible; use one of them or define `toolchain` which is compatible with the exec_group (already exists inside the `exec_group`)",
        ),
        (
            "a_shell_auto_name",
            "Invalid name for exec group '//eg:t1'.",
        ),
        (
            "a_egcw_rel",
            "Unable to find an execution platform for toolchains [//eg:t2opt] and target platform @@platforms//host:host from available execution platforms []",
        ),
        (
            "a_egcw_bad",
            "Tried to set execution constraints for non-existent exec groups on //eg:a_egcw_bad: //eg:t9 (did you mean '//eg:t1'?)",
        ),
        (
            "a_egcw_syntax",
            "Tried to set execution constraints for non-existent exec groups on //eg:a_egcw_syntax: //eg:a:b\n",
        ),
    ] {
        rejected(&f.build(&[&format!("//eg:{target}")]), message);
    }
    ok(&f.build(&["//eg:a_props"]));
    // Without the attribute, --incompatible_auto_exec_groups decides.
    let text = ok(&f.build(&["//eg:f"]));
    has_lines(
        &text,
        &[
            "AUTO1 <ctx.exec_groups: link> <toolchain_context.resolved_labels: //eg:t1, //eg:t2opt> t1_impl None True",
        ],
    );
    assert!(!text.contains("AUTO2"), "{text}");
    has_lines(
        &ok(&f.build(&["--incompatible_auto_exec_groups", "//eg:f"])),
        AUTO_PRINTS,
    );
    rejected(
        &f.build(&["--incompatible_auto_exec_groups", "//eg:f_run"]),
        TOOLCHAIN_UNSET,
    );
    ok(&f.build(&["--incompatible_auto_exec_groups", "//eg:o"]));
    // One context only: no toolchain parameter is needed.
    ok(&f.build(&["//eg:p_run", "//eg:p_tools"]));
    rejected(
        &f.build(&["--incompatible_auto_exec_groups=maybe", "//eg:f"]),
        "While parsing option --incompatible_auto_exec_groups=maybe: 'maybe' is not a boolean",
    );
    assert!(
        f.run_profile(
            true,
            "query",
            &["--incompatible_auto_exec_groups", "//eg:f"]
        )
        .status
        .success()
    );
}

#[test]
fn subrules_see_automatic_exec_group_toolchains() {
    let f = Fixture::new();
    let sub = "SUB <toolchain_context.resolved_labels: //eg:t1> t1_impl [Label(\"//eg:t1\")]";
    has_lines(&ok(&f.build(&["//eg:s_run"])), &["RULE t1_impl", sub]);
    for (target, message) in [
        (
            "s_exec_group",
            "'exec_group' may not be specified in subrules",
        ),
        (
            "s_toolchain",
            "'toolchain' may not be specified in subrules",
        ),
        (
            "s_toolchain_none",
            "'toolchain' may not be specified in subrules",
        ),
    ] {
        let text = rejected(&f.build(&[&format!("//eg:{target}")]), message);
        has_lines(&text, &[sub]);
    }
}

#[test]
fn the_default_test_exec_group_and_declared_overrides() {
    let f = Fixture::new();
    let test1 = "TEST1 <ctx.exec_groups: test> True <toolchain_context.resolved_labels: @@bazel_tools//tools/test:default_test_toolchain_type> struct() [Label(\"@@bazel_tools//tools/test:default_test_toolchain_type\")]";
    has_lines(&ok(&f.build(&["//eg:eg_test"])), &[test1]);
    has_lines(&ok(&f.build(&["//eg:eg_test_props"])), &[test1]);
    has_lines(
        &ok(&f.build(&["//eg:own_test"])),
        &[
            "OWN1 <ctx.exec_groups: test> <toolchain_context.resolved_labels: //eg:t1> t1_impl False",
        ],
    );
    has_lines(
        &ok(&f.build(&["--platforms=//eg:other", "//eg:cov_test"])),
        &[
            "DEP //eg:dep True",
            "DEP //eg:dep False",
            "COV <ctx.exec_groups: test>",
        ],
    );
    rejected(&f.build(&["//eg:auto_test_tools"]), TOOLCHAIN_UNSET);
    ok(&f.build(&["//eg:auto_test_shell"]));
    // Divergence D3: without the built-in profile the default test group
    // exists, but its toolchains need @bazel_tools.
    let flags = [
        "--host_platform=//eg:other",
        "--platforms=//eg:other",
        "//eg:eg_test",
    ];
    rejected(
        &f.run_profile(false, "build", &flags),
        "the toolchains of the default `test` exec group need the built-in @bazel_tools repository",
    );
    has_lines(
        &ok(&f.run_profile(
            false,
            "build",
            &[
                "--host_platform=//eg:other",
                "--platforms=//eg:other",
                "//eg:g",
            ],
        )),
        EG_PRINTS,
    );
}

#[test]
fn genuine_rule_set_declaration_shapes() {
    let f = Fixture::new();
    has_lines(
        &ok(&f.build(&["//eg:py", "//eg:go", "//eg:cc_test"])),
        &[
            "PY <ctx.exec_groups: //eg:cpp_toolchain_type, //eg:t1, //eg:t2opt, cpp_link, py_precompile> <toolchain_context.resolved_labels: //eg:t1, //eg:t2opt, //eg:cpp_toolchain_type> t1_impl None []",
            "GO <ctx.exec_groups: go_link> <toolchain_context.resolved_labels: //eg:t1, //eg:cpp_toolchain_type> t1_impl",
            "CC <ctx.exec_groups: //eg:cpp_toolchain_type, cpp_link, test> None <toolchain_context.resolved_labels: //eg:cpp_toolchain_type> True",
        ],
    );
    has_lines(
        &ok(&f.build(&["--platforms=//eg:other", "//eg:py_test"])),
        &[
            "DEP //eg:dep False",
            "PY <ctx.exec_groups: //eg:cpp_toolchain_type, //eg:t1, //eg:t2opt, cpp_link, py_precompile, test> <toolchain_context.resolved_labels: //eg:t1, //eg:t2opt, //eg:cpp_toolchain_type> t1_impl None []",
        ],
    );
    has_lines(
        &ok(&f.build(&["--incompatible_auto_exec_groups", "//eg:go"])),
        &[
            "GO <ctx.exec_groups: //eg:t1, go_link> <toolchain_context.resolved_labels: //eg:t1, //eg:cpp_toolchain_type> t1_impl",
        ],
    );
}
