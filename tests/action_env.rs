//! N42 CLI coverage (docs/ACTION_ENV.md): genrule and Starlark spawn
//! environments as Bazel 9.3 computes them (`--action_env`,
//! `--host_action_env` for exec tools, `use_default_shell_env`, `env`
//! overrides, `--incompatible_strict_action_env`, `--test_env`), inherited
//! values keyed, and option diagnostics. Expected environments follow the
//! paired genuine Bazel 9.3 capture (tests/fixtures/action_env).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

const DEFS: &str = r#"
def _dump_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        command = "env | LC_ALL=C sort > $1",
        arguments = [out.path],
        use_default_shell_env = ctx.attr.default,
        env = ctx.attr.env,
    )
    return [DefaultInfo(files = depset([out]))]

dump = rule(implementation = _dump_impl, attrs = {"default": attr.bool(), "env": attr.string_dict()})
"#;

const BUILD: &str = r#"load(":defs.bzl", "dump")
genrule(name = "gen", outs = ["gen.txt"], cmd = "env | LC_ALL=C sort > $@")
genrule(name = "tool", outs = ["tool.txt"], cmd = "env | LC_ALL=C sort > $@")
genrule(name = "via_tool", tools = [":tool"], outs = ["via_tool.txt"], cmd = "cat $(location :tool) > $@")
dump(name = "sl_default", default = True, env = {"X": "1"})
dump(name = "sl_plain", env = {"X": "1"})
dump(name = "sl_override", default = True, env = {"HOME": "/override", "TMPDIR": "/explicit"})
"#;

const TARGETS: [&str; 5] = [
    "//pkg:gen",
    "//pkg:via_tool",
    "//pkg:sl_default",
    "//pkg:sl_plain",
    "//pkg:sl_override",
];

const STRICT_PATH: &str = "PATH=/bin:/usr/bin:/usr/local/bin";

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("action-env-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        for (name, text) in [
            ("MODULE.bazel", "module(name='action_env_fixture')\n"),
            ("pkg/defs.bzl", DEFS),
            ("pkg/BUILD.bazel", BUILD),
            // An explicit execution platform for the exec-configured tool
            // (the default host platform needs @bazel_tools natively).
            (
                "p/BUILD.bazel",
                "platform(name = \"host\", visibility = [\"//visibility:public\"])\n",
            ),
        ] {
            let path = ws.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// Run the CLI with exactly `client` as its environment (plus a PATH to
    /// find `timeout` unless `client` sets one).
    fn run(&self, client: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = Command::new("/usr/bin/timeout");
        command
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .env_clear()
            .envs(verifier_environment());
        if !client.iter().any(|(name, _)| *name == "PATH") {
            command.env("PATH", "/usr/bin:/bin");
        }
        command.envs(client.iter().copied());
        let output = command.explicit_modules().output().unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    /// Build; return (output path below the output root, path) and stderr.
    fn build(&self, client: &[(&str, &str)], args: &[&str]) -> (Vec<(String, PathBuf)>, String) {
        let mut all = vec!["build", "--host_platform=//p:host"];
        all.extend(args);
        let output = self.run(client, &all);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        let outputs = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .filter_map(|(_, path)| {
                let relative = path.split_once("/bin/")?.1.to_owned();
                Some((relative, PathBuf::from(path)))
            })
            .collect();
        (outputs, stderr)
    }

    fn rejected(&self, args: &[&str], diagnostic: &str) {
        let output = self.run(&[], args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "unexpected success: {args:?}");
        assert!(stderr.contains(diagnostic), "{args:?}: {stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

/// The `env | sort` lines without bash's own PWD/SHLVL/`_`, with an
/// absolute TMPDIR normalized to `TMPDIR=<tmp>` (its value is
/// intentionally unstable natively, docs/ACTION_ENV.md).
fn environment(outputs: &[(String, PathBuf)], name: &str) -> Vec<String> {
    let path = &outputs
        .iter()
        .find(|(relative, _)| relative == name)
        .unwrap_or_else(|| panic!("no output {name} in {outputs:?}"))
        .1;
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !["PWD=", "SHLVL=", "_="].iter().any(|p| line.starts_with(p)))
        .map(|line| match line.strip_prefix("TMPDIR=") {
            Some(tmp) => {
                assert!(tmp.starts_with('/') && tmp != "/explicit", "{line}");
                "TMPDIR=<tmp>".to_owned()
            }
            None => line.to_owned(),
        })
        .collect()
}

fn lines(expected: &[&str]) -> Vec<String> {
    expected.iter().map(|line| (*line).to_owned()).collect()
}

fn summary(stderr: &str) -> &str {
    stderr
        .lines()
        .rfind(|line| line.starts_with("bazel-rs: "))
        .unwrap_or_default()
}

const CLIENT: [(&str, &str); 3] = [("HOME", "/home/n42"), ("USER", "n42"), ("FOO", "client")];

#[test]
fn default_environments_follow_the_configuration_shell_environment() {
    let ws = Workspace::new();
    for extra in [&[][..], &["-c", "opt"][..]] {
        let mut args = extra.to_vec();
        args.extend(TARGETS);
        let (outputs, _) = ws.build(&CLIENT, &args);
        // GenRuleBase.java:210-219 with SHELL_ACTION_ENV: only the strict
        // PATH (no LANG/LC_ALL, nothing from the client).
        for genrule in ["pkg/gen.txt", "pkg/via_tool.txt"] {
            assert_eq!(
                environment(&outputs, genrule),
                lines(&[STRICT_PATH, "TMPDIR=<tmp>"]),
                "{genrule} {extra:?}"
            );
        }
        assert_eq!(
            environment(&outputs, "pkg/sl_default.txt"),
            lines(&[STRICT_PATH, "TMPDIR=<tmp>", "X=1"])
        );
        assert_eq!(
            environment(&outputs, "pkg/sl_plain.txt"),
            lines(&["TMPDIR=<tmp>", "X=1"])
        );
        // An action TMPDIR never reaches the process (PosixLocalEnvProvider).
        assert_eq!(
            environment(&outputs, "pkg/sl_override.txt"),
            lines(&["HOME=/override", STRICT_PATH, "TMPDIR=<tmp>"])
        );
    }
}

#[test]
fn action_env_and_host_action_env_shape_target_and_exec_environments() {
    let ws = Workspace::new();
    let mut args = vec![
        "--action_env=FOO=bar",
        "--action_env=HOME",
        "--action_env==PATH",
        "--action_env=NOT_IN_CLIENT",
        "--action_env",
        "USER",
        "--host_action_env=HOST=1",
        "--host_action_env=USER",
    ];
    args.extend(TARGETS);
    let (outputs, _) = ws.build(&CLIENT, &args);
    // `=PATH` removes the strict default; `HOME`/`USER` are inherited, a name
    // the client does not set is absent; FOO=bar beats the client's FOO.
    assert_eq!(
        environment(&outputs, "pkg/gen.txt"),
        lines(&["FOO=bar", "HOME=/home/n42", "TMPDIR=<tmp>", "USER=n42"])
    );
    // The exec tool's environment is --host_action_env over the strict PATH.
    assert_eq!(
        environment(&outputs, "pkg/via_tool.txt"),
        lines(&["HOST=1", STRICT_PATH, "TMPDIR=<tmp>", "USER=n42"])
    );
    assert_eq!(
        environment(&outputs, "pkg/sl_default.txt"),
        lines(&[
            "FOO=bar",
            "HOME=/home/n42",
            "TMPDIR=<tmp>",
            "USER=n42",
            "X=1"
        ])
    );
    assert_eq!(
        environment(&outputs, "pkg/sl_plain.txt"),
        lines(&["TMPDIR=<tmp>", "X=1"])
    );
    // SpawnAction.createActionEnvironment: `env` overrides an inherited name.
    assert_eq!(
        environment(&outputs, "pkg/sl_override.txt"),
        lines(&["FOO=bar", "HOME=/override", "TMPDIR=<tmp>", "USER=n42"])
    );
    // The last entry per name wins.
    let (outputs, _) = ws.build(
        &CLIENT,
        &[
            "--action_env=A=1",
            "--action_env=B=2",
            "--action_env==A",
            "//pkg:gen",
        ],
    );
    assert_eq!(
        environment(&outputs, "pkg/gen.txt"),
        lines(&["B=2", STRICT_PATH, "TMPDIR=<tmp>"])
    );
}

#[test]
fn inherited_client_values_are_keyed() {
    let ws = Workspace::new();
    let build = |home: Option<&str>| {
        let client: Vec<(&str, &str)> = home.map(|home| ("HOME", home)).into_iter().collect();
        ws.build(&client, &["--action_env=HOME", "//pkg:gen"])
    };
    let (outputs, stderr) = build(Some("/a"));
    assert!(
        summary(&stderr).contains("1 actions executed, 0 cache hits"),
        "{stderr}"
    );
    assert!(environment(&outputs, "pkg/gen.txt").contains(&"HOME=/a".to_owned()));
    let (_, stderr) = build(Some("/a"));
    assert!(
        summary(&stderr).contains("0 actions executed, 1 cache hits"),
        "{stderr}"
    );
    let (outputs, stderr) = build(Some("/b"));
    assert!(
        summary(&stderr).contains("1 actions executed, 0 cache hits"),
        "{stderr}"
    );
    assert!(environment(&outputs, "pkg/gen.txt").contains(&"HOME=/b".to_owned()));
    let (outputs, stderr) = build(None);
    assert!(
        summary(&stderr).contains("1 actions executed, 0 cache hits"),
        "{stderr}"
    );
    assert_eq!(
        environment(&outputs, "pkg/gen.txt"),
        lines(&[STRICT_PATH, "TMPDIR=<tmp>"])
    );
    // A fixed value is keyed by value.
    let (_, stderr) = ws.build(&[], &["--action_env=HOME=/a", "//pkg:gen"]);
    assert!(
        summary(&stderr).contains("1 actions executed, 0 cache hits"),
        "{stderr}"
    );
}

#[test]
fn non_strict_action_env_inherits_path_and_ld_library_path() {
    let ws = Workspace::new();
    for flag in [
        "--noincompatible_strict_action_env",
        "--incompatible_strict_action_env=false",
        "--noexperimental_strict_action_env",
    ] {
        let (outputs, _) = ws.build(
            &[
                ("PATH", "/usr/bin:/bin:/client"),
                ("LD_LIBRARY_PATH", "/lib/x"),
            ],
            &[flag, "//pkg:gen", "//pkg:via_tool"],
        );
        // Propagated to the exec configuration (builtin_exec_platforms.bzl:203-207).
        for genrule in ["pkg/gen.txt", "pkg/via_tool.txt"] {
            assert_eq!(
                environment(&outputs, genrule),
                lines(&[
                    "LD_LIBRARY_PATH=/lib/x",
                    "PATH=/usr/bin:/bin:/client",
                    "TMPDIR=<tmp>"
                ]),
                "{flag} {genrule}"
            );
        }
    }
}

#[test]
fn test_env_is_recorded_without_changing_action_keys() {
    let ws = Workspace::new();
    let (_, stderr) = ws.build(&CLIENT, &TARGETS);
    assert!(
        summary(&stderr).contains("6 actions executed, 0 cache hits"),
        "{stderr}"
    );
    let mut args = vec!["--test_env=A=1", "--test_env=HOME", "--test_env==B"];
    args.extend(TARGETS);
    let (_, stderr) = ws.build(&CLIENT, &args);
    assert!(
        summary(&stderr).contains("0 actions executed, 6 cache hits"),
        "{stderr}"
    );
}

#[test]
fn option_diagnostics_follow_the_env_vars_converter() {
    let ws = Workspace::new();
    let message =
        "Variable definitions must be in the form of a 'name=value', 'name', or '=name' assignment";
    for (args, diagnostic) in [
        (
            &["build", "--action_env=", "//pkg:gen"][..],
            format!("While parsing option --action_env=: {message}"),
        ),
        (
            &["build", "--host_action_env==", "//pkg:gen"][..],
            format!("While parsing option --host_action_env==: {message}"),
        ),
        (
            &["build", "--test_env", "", "//pkg:gen"][..],
            format!("While parsing option --test_env : {message}"),
        ),
        (
            &["build", "--noaction_env", "//pkg:gen"][..],
            "--noaction_env :: Illegal use of 'no' prefix on non-boolean option: --noaction_env"
                .to_owned(),
        ),
        (
            &[
                "build",
                "--incompatible_strict_action_env=maybe",
                "//pkg:gen",
            ][..],
            "'maybe' is not a boolean".to_owned(),
        ),
        (
            &["query", "--test_env=A", "//pkg:gen"][..],
            "--test_env=A :: Unrecognized option: --test_env=A".to_owned(),
        ),
        (
            &["query", "--incompatible_strict_action_env", "//pkg:gen"][..],
            "Unrecognized option: --incompatible_strict_action_env".to_owned(),
        ),
    ] {
        ws.rejected(args, &diagnostic);
    }
    // `query` registers CoreOptions, so it accepts (and ignores) --action_env.
    let output = ws.run(
        &[],
        &[
            "query",
            "--action_env=A=1",
            "--host_action_env=B",
            "//pkg:gen",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "//pkg:gen\n");
}

/// N23c's `ctx.configuration.default_shell_env` is the configured target's
/// fixed action environment (BuildConfigurationValue.getLocalShellEnvironment,
/// N42 merge): `--action_env` in target configurations, `--host_action_env`
/// in exec configurations. Expected values are the paired capture's
/// REFERENCE_ONLY rows (tests/fixtures/action_env/reference_receipt.json,
/// `cfg/shell_env.json` and `cfg/via_cfg_tool.json`, `json.encode`).
#[test]
fn default_shell_env_is_the_fixed_part() {
    let ws = Workspace::new();
    for (name, text) in [
        (
            "cfg/defs.bzl",
            r#"
Shell = provider(fields = ["text"])

def _shell_impl(ctx):
    text = json.encode(ctx.configuration.default_shell_env)
    if ctx.attr.tool:
        text = ctx.attr.tool[Shell].text
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, text + "\n")
    return [DefaultInfo(files = depset([out])), Shell(text = text)]

shell = rule(implementation = _shell_impl, attrs = {"tool": attr.label(cfg = "exec")})
"#,
        ),
        (
            "cfg/BUILD.bazel",
            "load(':defs.bzl', 'shell')\nshell(name = 'shell_env')\nshell(name = 'tool')\nshell(name = 'via_cfg_tool', tool = ':tool')\n",
        ),
    ] {
        let path = ws.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    const STRICT: &str = r#"{"PATH":"/bin:/usr/bin:/usr/local/bin"}"#;
    for (flags, target, exec) in [
        (&[][..], STRICT, STRICT),
        (
            &["--action_env=FOO=bar"][..],
            r#"{"FOO":"bar","PATH":"/bin:/usr/bin:/usr/local/bin"}"#,
            STRICT,
        ),
        (
            &[
                "--action_env=HOME",
                "--action_env=USER",
                "--action_env=NOT_IN_CLIENT",
            ][..],
            STRICT,
            STRICT,
        ),
        (&["--action_env==PATH"][..], "{}", STRICT),
        (
            &[
                "--action_env=FOO=bar",
                "--action_env=HOME",
                "--action_env==PATH",
                "--action_env=A=1",
                "--action_env=B=2",
                "--action_env==A",
            ][..],
            r#"{"B":"2","FOO":"bar"}"#,
            STRICT,
        ),
        (
            &[
                "--host_action_env=HOST=1",
                "--host_action_env=USER",
                "--action_env=TARGET=1",
            ][..],
            r#"{"PATH":"/bin:/usr/bin:/usr/local/bin","TARGET":"1"}"#,
            r#"{"HOST":"1","PATH":"/bin:/usr/bin:/usr/local/bin"}"#,
        ),
        (&["--host_action_env==PATH"][..], STRICT, "{}"),
        (
            &["--action_env=TMPDIR=/explicit", "--action_env=EMPTY="][..],
            r#"{"EMPTY":"","PATH":"/bin:/usr/bin:/usr/local/bin","TMPDIR":"/explicit"}"#,
            STRICT,
        ),
        (&["--noincompatible_strict_action_env"][..], "{}", "{}"),
        (&["--test_env=T=1", "--test_env=HOME"][..], STRICT, STRICT),
    ] {
        let mut args = flags.to_vec();
        args.extend(["//cfg:shell_env", "//cfg:via_cfg_tool"]);
        let (outputs, _) = ws.build(&CLIENT, &args);
        assert_eq!(
            environment(&outputs, "cfg/shell_env.json"),
            [target],
            "{flags:?}"
        );
        assert_eq!(
            environment(&outputs, "cfg/via_cfg_tool.json"),
            [exec],
            "{flags:?}"
        );
    }
}
