//! CLI coverage for rc-file evaluation (docs/BAZELRC.md): rc discovery and
//! startup options, parsing, imports, command groups, `--config` expansion,
//! `common` ignoring, `--announce_rc`, and explicit failures naming the rc
//! file:line of options the native profile does not model. Expected texts
//! follow the pinned Bazel 9.3 sources cited in src/bazelrc.rs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace {
    root: tempfile::TempDir,
    home: tempfile::TempDir,
}

const DEFS: &str = r#"
def _flag_impl(ctx):
    return []

bool_flag = rule(implementation = _flag_impl, build_setting = config.bool(flag = True))
"#;

const BUILD: &str = r#"
load(":defs.bzl", "bool_flag")
bool_flag(name = "flag", build_setting_default = False)
config_setting(name = "opt", values = {"compilation_mode": "opt"})
config_setting(name = "dbg", values = {"compilation_mode": "dbg"})
config_setting(name = "x", define_values = {"x": "1"})
config_setting(name = "flag_on", flag_values = {":flag": "True"})
genrule(
    name = "mode",
    outs = ["mode.txt"],
    cmd = select({":opt": "echo opt > $@", ":dbg": "echo dbg > $@", "//conditions:default": "echo fastbuild > $@"}),
)
genrule(
    name = "define",
    outs = ["define.txt"],
    cmd = select({":x": "echo x > $@", "//conditions:default": "echo none > $@"}),
)
genrule(
    name = "flagged",
    outs = ["flagged.txt"],
    cmd = select({":flag_on": "echo on > $@", "//conditions:default": "echo off > $@"}),
)
filegroup(name = "ok")
"#;

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self {
            root: tempfile::Builder::new()
                .prefix("bazelrc-")
                .tempdir_in(&base)
                .unwrap(),
            home: tempfile::Builder::new()
                .prefix("bazelrc-home-")
                .tempdir_in(&base)
                .unwrap(),
        };
        ws.file("MODULE.bazel", "module(name='rc')\n");
        ws.file("defs.bzl", DEFS);
        ws.file("BUILD.bazel", BUILD);
        ws
    }

    fn path(&self) -> PathBuf {
        fs::canonicalize(self.root.path()).unwrap()
    }

    fn home(&self) -> PathBuf {
        fs::canonicalize(self.home.path()).unwrap()
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.root.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn command(&self, startup: &[&str], args: &[&str]) -> Command {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--nosystem_rc", "--cache-dir=private-cache"])
            .args(startup)
            .args(args)
            .env("HOME", self.home())
            .env_remove("BAZELRC")
            .env_remove("COLUMNS")
            .env_remove("EMACS")
            .env_remove("INSIDE_EMACS")
            .current_dir(self.path());
        command
    }

    fn run_with(&self, startup: &[&str], args: &[&str]) -> Output {
        let output = self.command(startup, args).output().unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(&[], args)
    }

    fn built(&self, startup: &[&str], args: &[&str]) -> String {
        let output = self.run_with(startup, args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let line = stdout.lines().next().expect("built output");
        fs::read_to_string(line.split_once('\t').unwrap().1)
            .unwrap()
            .trim()
            .to_owned()
    }

    fn failed(&self, startup: &[&str], args: &[&str]) -> String {
        let output = self.run_with(startup, args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains("panicked"), "{}", stderr(&output));
        stderr(&output)
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn rc_path(ws: &Workspace, name: &str) -> String {
    ws.path().join(name).display().to_string()
}

#[test]
fn rc_options_apply_and_the_command_line_overrides_them() {
    let ws = Workspace::new();
    ws.file(
        ".bazelrc",
        "build -c opt\nbuild:dbg -c dbg\nbuild:x --define=x=1\n",
    );
    assert_eq!(ws.built(&[], &["build", ":mode"]), "opt");
    assert_eq!(
        ws.built(&[], &["build", "-c", "fastbuild", ":mode"]),
        "fastbuild"
    );
    // An expansion takes the place of its --config (ConfigExpander.java:110-131).
    assert_eq!(
        ws.built(&[], &["build", "--config=dbg", "-c", "opt", ":mode"]),
        "opt"
    );
    assert_eq!(
        ws.built(&[], &["build", "-c", "opt", "--config", "dbg", ":mode"]),
        "dbg"
    );
    assert_eq!(ws.built(&[], &["build", "--config=x", ":define"]), "x");
    assert_eq!(
        ws.built(&["--noworkspace_rc"], &["build", ":mode"]),
        "fastbuild"
    );
    assert_eq!(
        ws.built(&["--ignore_all_rc_files"], &["build", ":mode"]),
        "fastbuild"
    );
}

#[test]
fn groups_inherit_in_order_and_common_ignores_other_commands_options() {
    let ws = Workspace::new();
    ws.file(
        ".bazelrc",
        "build -c dbg\ncommon -c opt\ncommon --test_output=errors\ntest --bogus\nquery --bogus\nalways --define=x=1\n",
    );
    // build options override common ones regardless of order.
    assert_eq!(
        ws.built(&["--noworkspace_rc"], &["build", ":mode"]),
        "fastbuild"
    );
    let out = ws.run(&["build", ":mode"]);
    assert!(
        stderr(&out).contains("unsupported option \"--test_output=errors\" from rc file"),
        "{}",
        stderr(&out)
    );
    ws.file(
        ".bazelrc",
        "build -c dbg\ncommon -c opt\ncommon --output=label\nalways --define=x=1\n",
    );
    assert_eq!(ws.built(&[], &["build", ":mode"]), "dbg");
    assert_eq!(ws.built(&[], &["build", ":define"]), "x");
    // --output is a query option: native query does not model it.
    let error = ws.failed(&[], &["query", ":ok"]);
    assert!(
        error.contains(&format!(
            "unsupported option \"--output=label\" from rc file {}:3 ('common' options); options are never silently ignored",
            rc_path(&ws, ".bazelrc")
        )),
        "{error}"
    );
    // `always` options are not ignored for commands without them.
    ws.file(".bazelrc", "always --test_output=errors\n");
    let error = ws.failed(&[], &["query", ":ok"]);
    assert!(error.contains("--test_output=errors"), "{error}");
}

#[test]
fn unmodelled_rc_options_name_their_file_and_line() {
    let ws = Workspace::new();
    ws.file(
        ".bazelrc",
        "# comment\nbuild \\\n  --remote_cache=https://example.invalid\nbuild:cross --sandbox_add_mount_pair=/tmp\ncommon --repo_env=SECRET=value\n",
    );
    let error = ws.failed(&[], &["query", ":ok"]);
    assert!(
        error.contains(&format!(
            "unsupported option --repo_env from rc file {}:5 ('common' options); repository environment overrides are not implemented",
            rc_path(&ws, ".bazelrc")
        )),
        "{error}"
    );
    assert!(!error.contains("SECRET"), "{error}");
    ws.file(
        ".bazelrc",
        "# comment\nbuild \\\n  --remote_cache=https://example.invalid\nbuild:cross --sandbox_add_mount_pair=/tmp\n",
    );
    let error = ws.failed(&[], &["build", ":ok"]);
    assert!(
        error.contains(&format!(
            "unsupported option \"--remote_cache=https://example.invalid\" from rc file {}:3 ('build' options)",
            rc_path(&ws, ".bazelrc")
        )),
        "{error}"
    );
    let error = ws.failed(
        &["--noworkspace_rc", "--bazelrc=.bazelrc"],
        &["build", "--config=cross", ":ok"],
    );
    assert!(error.contains("--remote_cache"), "{error}");
    ws.file(".bazelrc", "build:cross --sandbox_add_mount_pair=/tmp\n");
    let error = ws.failed(&[], &["build", "--config=cross", ":ok"]);
    assert!(
        error.contains(&format!(
            "unsupported option \"--sandbox_add_mount_pair=/tmp\" from rc file {}:1 (config definition 'build:cross')",
            rc_path(&ws, ".bazelrc")
        )),
        "{error}"
    );
}

#[test]
fn config_errors_are_bazel_errors() {
    let ws = Workspace::new();
    ws.file(
        ".bazelrc",
        "build:a --config=b\nbuild:b --config=a\nquery:q -c opt\n",
    );
    let error = ws.failed(&[], &["build", "--config=nope", ":ok"]);
    assert!(
        error.ends_with("bazel-rs: Config value 'nope' is not defined in any .rc file\n"),
        "{error}"
    );
    let error = ws.failed(&[], &["build", "--config=a", ":ok"]);
    assert!(
        error.contains("Config expansion has a cycle: config value a expands to itself, see inheritance chain [a, b]"),
        "{error}"
    );
    // build:NAME definitions do not apply to query.
    let error = ws.failed(&[], &["build", "--config=q", ":ok"]);
    assert!(
        error.contains("Config value 'q' is not defined in any .rc file"),
        "{error}"
    );
    assert!(ws.run(&["query", "--config=q", ":ok"]).status.success());
}

#[test]
fn rc_discovery_imports_and_bazelrc_flags() {
    let ws = Workspace::new();
    fs::write(ws.home().join(".bazelrc"), "build -c dbg\n").unwrap();
    ws.file("extra.rc", "build --define=x=1\n");
    ws.file(".bazelrc", "import %workspace%/extra.rc\ntry-import %workspace%/missing.rc\ntry-import /nonexistent/x.rc\n");
    assert_eq!(ws.built(&[], &["build", ":mode"]), "dbg");
    assert_eq!(ws.built(&[], &["build", ":define"]), "x");
    assert_eq!(ws.built(&["--nohome_rc"], &["build", ":mode"]), "fastbuild");
    ws.file("opt.rc", "build -c opt\n");
    assert_eq!(ws.built(&["--bazelrc=opt.rc"], &["build", ":mode"]), "opt");
    // Values after --bazelrc=/dev/null are ignored (blaze_util.cc:82-114).
    assert_eq!(
        ws.built(
            &["--bazelrc=/dev/null", "--bazelrc=opt.rc"],
            &["build", ":mode"]
        ),
        "dbg"
    );
    let mut command = ws.command(&[], &["build", ":mode"]);
    let output = command.env("BAZELRC", "opt.rc").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let error = ws.failed(&["--bazelrc=missing.rc"], &["build", ":ok"]);
    assert!(
        error.contains(&format!(
            "Error: Unable to read .bazelrc file '{}'.",
            rc_path(&ws, "missing.rc")
        )),
        "{error}"
    );
    ws.file(".bazelrc", "import %workspace%/missing.rc\n");
    let error = ws.failed(&[], &["build", ":ok"]);
    assert!(
        error.contains(&format!(
            "Nonexistent path in import declaration in config file '{}': 'import %workspace%/missing.rc' (are you in your source checkout/WORKSPACE?)",
            rc_path(&ws, ".bazelrc")
        )),
        "{error}"
    );
    ws.file(".bazelrc", "import %workspace%/loop.rc\n");
    ws.file("loop.rc", "import %workspace%/.bazelrc\n");
    let error = ws.failed(&[], &["build", ":ok"]);
    let (root, loop_rc) = (rc_path(&ws, ".bazelrc"), rc_path(&ws, "loop.rc"));
    assert!(
        error.contains(&format!(
            "Import loop detected:\n  {root}\n  {loop_rc}\n  {root}\n"
        )),
        "{error}"
    );
    ws.file(".bazelrc", "import a b\n");
    let error = ws.failed(&[], &["build", ":ok"]);
    assert!(
        error.contains("Invalid import declaration in config file"),
        "{error}"
    );
    ws.file(".bazelrc", "try-import-if-bazel-version >=9.0.0 %workspace%/opt.rc\ntry-import-if-bazel-version <9.0.0 %workspace%/extra.rc\n");
    assert_eq!(ws.built(&["--nohome_rc"], &["build", ":mode"]), "opt");
    assert_eq!(ws.built(&["--nohome_rc"], &["build", ":define"]), "none");
}

#[test]
fn duplicate_rc_files_warn() {
    let ws = Workspace::new();
    ws.file("extra.rc", "build -c opt\n");
    ws.file(".bazelrc", "import %workspace%/extra.rc\n");
    let output = ws.run_with(&["--bazelrc=extra.rc"], &["build", ":mode"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!(
            "WARNING: Duplicate rc file: {} is read multiple times, it is a standard rc file location but must have been unnecessarily imported earlier.",
            rc_path(&ws, "extra.rc")
        )),
        "{}",
        stderr(&output)
    );
}

#[test]
fn startup_options_in_rc_files() {
    let ws = Workspace::new();
    ws.file(".bazelrc", "startup --nohome_rc\n");
    let error = ws.failed(&[], &["build", ":ok"]);
    assert_eq!(
        error,
        format!(
            "INFO: Reading 'startup' options from {}: --nohome_rc\nbazel-rs: Can't specify --nohome_rc in the .bazelrc file.\n",
            rc_path(&ws, ".bazelrc")
        )
    );
    ws.file(".bazelrc", "startup --bazelrc=x\n");
    assert!(
        ws.failed(&[], &["build", ":ok"])
            .contains("Can't specify --bazelrc in the RC file.")
    );
    ws.file(".bazelrc", "\nstartup --output_base=/tmp/x\n");
    let error = ws.failed(&[], &["build", ":ok"]);
    assert!(
        error.contains(&format!(
            "unsupported startup option \"--output_base=/tmp/x\" from rc file {}:2",
            rc_path(&ws, ".bazelrc")
        )),
        "{error}"
    );
    ws.file(".bazelrc", "startup --bogus\n");
    assert!(
        ws.failed(&[], &["build", ":ok"])
            .contains("Unknown startup option: '--bogus'.")
    );
    let error = ws.failed(&["--batch"], &["build", ":ok"]);
    assert!(
        error.contains("Unknown startup option: '--bogus'."),
        "{error}"
    );
    ws.file(".bazelrc", "");
    assert!(
        ws.failed(&["--batch"], &["build", ":ok"])
            .contains("unsupported startup option \"--batch\"")
    );
    let output = ws.run_with(&["--ignore_all_rc_files", "--bazelrc=x"], &["query", ":ok"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output)
            .contains("WARNING: Value of --bazelrc is ignored, since --ignore_all_rc_files is on.")
    );
}

#[test]
fn announce_rc_and_rc_residue() {
    let ws = Workspace::new();
    ws.file(
        ".bazelrc",
        "common --announce_rc\nbuild :mode\ncommon --output=label\nbuild:o -c opt\n",
    );
    let output = ws.run(&["build", "--config=o"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let rc = rc_path(&ws, ".bazelrc");
    let expected = format!(
        "INFO: Options provided by the client:\n  Inherited 'common' options: --isatty=0 --terminal_columns=80\nINFO: Reading rc options for 'build' from {rc}:\n  Inherited 'common' options: --announce_rc --output=label\n  Ignored as unsupported by 'build': --output=label\nINFO: Reading rc options for 'build' from {rc}:\n  'build' options: :mode\nINFO: Found applicable config definition build:o in file {rc}: -c opt\n"
    );
    assert!(
        stderr(&output).starts_with(&expected),
        "{}",
        stderr(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout
        .lines()
        .next()
        .unwrap()
        .split_once('\t')
        .unwrap()
        .1
        .to_owned();
    assert_eq!(fs::read_to_string(Path::new(&path)).unwrap().trim(), "opt");
}

#[test]
fn starlark_flags_and_bad_options_from_rc() {
    let ws = Workspace::new();
    ws.file(".bazelrc", "build --//:flag\n");
    assert_eq!(ws.built(&[], &["build", ":flagged"]), "on");
    assert_eq!(ws.built(&[], &["build", "--no//:flag", ":flagged"]), "off");
    ws.file(".bazelrc", "query --zzqqxx_not_an_option\n");
    let error = ws.failed(&[], &["query", ":ok"]);
    let rc = rc_path(&ws, ".bazelrc");
    assert_eq!(
        error,
        format!(
            "INFO: Reading rc options for 'query' from {rc}:\n  'query' options: --zzqqxx_not_an_option\nbazel-rs: --zzqqxx_not_an_option :: Unrecognized option: --zzqqxx_not_an_option\n"
        )
    );
    // build reports unrecognized options in its second parsing round, without notes.
    ws.file(".bazelrc", "build --zzqqxx_not_an_option\n");
    let error = ws.failed(&[], &["build", ":ok"]);
    assert_eq!(
        error,
        "bazel-rs: --zzqqxx_not_an_option :: Unrecognized option: --zzqqxx_not_an_option\n"
    );
    ws.file(".bazelrc", "bulid -c opt\n");
    let output = ws.run(&["build", ":mode"]);
    assert!(stderr(&output).contains(&format!(
        "WARNING: while reading option defaults file '{rc}':\n  invalid command name 'bulid'."
    )));
}

#[test]
fn native_only_options_are_rejected_in_rc_files() {
    let ws = Workspace::new();
    let rc = rc_path(&ws, ".bazelrc");
    for (text, command, line) in [
        ("\nbuild --experimental-builtin-modules\n", "build", 2),
        ("common --experimental-builtin-modules=false\n", "query", 1),
        ("common --experimental-builtin-modules\n", "mod", 1),
        ("build --cache-dir=elsewhere\n", "build", 1),
    ] {
        ws.file(".bazelrc", text);
        let args: &[&str] = match command {
            "mod" => &["mod", "graph", "--output=json"],
            "query" => &["query", ":ok"],
            _ => &["build", ":ok"],
        };
        let error = ws.failed(&[], args);
        assert!(
            error.contains(&format!(" from rc file {rc}:{line} ")),
            "{error}"
        );
        assert!(
            error.contains("is not a Bazel option and is rejected in rc files"),
            "{error}"
        );
        assert!(error.contains("pass it on the command line"), "{error}");
    }
    ws.file(".bazelrc", "startup --cache-dir=elsewhere\n");
    let error = ws.failed(&[], &["build", ":ok"]);
    assert!(
        error.contains(&format!(
            "native-only option --cache-dir from rc file {rc}:1 is not a Bazel option"
        )),
        "{error}"
    );
    // Command-line use is unchanged.
    ws.file(".bazelrc", "");
    let output = ws.run(&["query", "--experimental-builtin-modules=false", ":ok"]);
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn announce_rc_redacts_environment_values() {
    let ws = Workspace::new();
    ws.file(
        ".bazelrc",
        "common --announce_rc\ncommon --test_env=TOKEN=secret --test_env PLAIN=hidden\n",
    );
    let output = ws.run(&["query", ":ok"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let notes = stderr(&output);
    assert!(
        notes.contains("Inherited 'common' options: --announce_rc --test_env=TOKEN=<redacted> --test_env PLAIN=<redacted>"),
        "{notes}"
    );
    assert!(
        notes.contains("Ignored as unsupported by 'query': --test_env=TOKEN=<redacted> --test_env PLAIN=<redacted>"),
        "{notes}"
    );
    assert!(
        !notes.contains("secret") && !notes.contains("hidden"),
        "{notes}"
    );
}
