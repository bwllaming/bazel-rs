//! The `run` command (N23f, docs/RUNFILES_TREES.md): the command line,
//! environment and working directory RunCommand builds for the target
//! (runtime/commands/RunCommand.java:688-912, RunCommandLine.java) and the
//! `--script_path` script (RunCommandLine.LinuxFormatter.getScriptForm).
//!
//! Paths below are under src/main/java/com/google/devtools/build/lib/ of the
//! pinned Bazel 9.3 sources.

use std::collections::{BTreeMap, BTreeSet};

/// RunCommand.ENV_VARIABLES_TO_CLEAR_UNCONDITIONALLY (RunCommand.java:214-224):
/// the runfiles library variables that would mislead the top-level binary.
pub const ENV_TO_CLEAR: [&str; 5] = [
    "JAVA_RUNFILES",
    "RUNFILES_DIR",
    "RUNFILES_MANIFEST_FILE",
    "RUNFILES_MANIFEST_ONLY",
    "TEST_SRCDIR",
];

/// `--run_env` values (Converters.EnvVarsConverter): `NAME=VALUE` sets,
/// `NAME` inherits from the client environment, `=NAME` unsets. One parser
/// for every EnvVarsConverter option (N42's crate::action_env::EnvVar).
pub use crate::action_env::EnvVar;

/// The `run` options (RunCommand.RunOptions, RunCommand.java:121-207;
/// CoreOptions.runUnder; the host shell, ShToolchain.getPathForHost).
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub script_path: Option<String>,
    /// `--run` (false: build and print the command line only).
    pub run: bool,
    pub run_env: Vec<EnvVar>,
    pub run_in_cwd: bool,
    pub omit_run_args: bool,
    /// `--run_under` (CoreOptions.runUnder, RunUnderConverter).
    pub run_under: Option<RunUnder>,
    /// `--shell_executable` (default /bin/bash, the Linux sh toolchain).
    pub shell: String,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            script_path: None,
            run: true,
            run_env: Vec::new(),
            run_in_cwd: false,
            omit_run_args: true,
            run_under: None,
            shell: "/bin/bash".to_owned(),
        }
    }
}

/// One `--run_under` value (analysis/config/RunUnder.java, parsed by
/// RunUnderConverter, analysis/config/RunUnderConverter.java:33-57): the
/// whole value, the first shell token (a label when it starts with `//` or
/// `@`, LabelRunUnder; otherwise a command, CommandRunUnder) and the
/// remaining tokens (`options`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunUnder {
    pub value: String,
    pub first: String,
    pub options: Vec<String>,
}

impl RunUnder {
    /// RunUnderConverter.convert: `Err` is the converter's message (the
    /// caller prefixes `While parsing option --run_under=VALUE: `).
    pub fn parse(value: &str) -> Result<Self, String> {
        let tokens = crate::ctx_surface::tokenize(value)
            .map_err(|error| format!("Not a valid command prefix {error}"))?;
        let Some((first, options)) = tokens.split_first() else {
            return Err("Empty command".to_owned());
        };
        Ok(RunUnder {
            value: value.to_owned(),
            first: first.clone(),
            options: options.to_vec(),
        })
    }

    /// LabelRunUnder: the first word starts with `//` or `@`.
    pub fn is_label(&self) -> bool {
        self.first.starts_with("//") || self.first.starts_with('@')
    }

    /// TestTargetExecutionSettings.needsShell (analysis/test/TestTargetExecutionSettings.java
    /// :167-178): a command without `/` (a shell built-in or a PATH lookup)
    /// runs through the shell.
    pub fn needs_shell(&self) -> bool {
        !self.is_label() && !self.first.contains('/')
    }

    /// The prefix of a test's command line (TestStrategy.addRunUnderArgs,
    /// analysis/test/TestStrategy.java:240-270): the label's executable
    /// (`label_executable`, its runfiles path) or the command (wrapped as
    /// `SHELL -c "$@" SHELL` when it needs the shell), then the options.
    pub fn test_prefix(&self, label_executable: Option<&str>, shell: &str) -> Vec<String> {
        let mut args = Vec::new();
        if self.is_label() {
            args.extend(label_executable.map(str::to_owned));
        } else {
            if self.needs_shell() {
                args.extend([
                    shell.to_owned(),
                    "-c".to_owned(),
                    "\"$@\"".to_owned(),
                    shell.to_owned(),
                ]);
            }
            args.push(self.first.clone());
        }
        args.extend(self.options.iter().cloned());
        args
    }
}

/// ShellEscaper.escapeString (lib/util/ShellEscaper.java:67-84).
pub fn shell_escape(text: &str) -> String {
    if !crate::command_line::shell_quote_required(text) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The process RunCommand asks the client to execute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunCommandLine {
    /// Executable path, then the target's `args`, then the command line's.
    pub args: Vec<String>,
    /// How many of `args` came from the command line (residue).
    pub residue: usize,
    pub run_under: Option<String>,
    /// The run environment (TreeMap, sorted by name).
    pub environment: BTreeMap<String, String>,
    /// The variables to clear (sorted).
    pub clear: BTreeSet<String>,
    pub working_directory: String,
}

/// The inputs of [`command_line`].
pub struct RunInputs<'a> {
    pub workspace: &'a str,
    pub working_directory: &'a str,
    pub execroot: &'a str,
    pub build_id: &'a str,
    pub client_env: &'a BTreeMap<String, String>,
    pub options: &'a RunOptions,
    /// The target's RunEnvironmentInfo (fixed, inherited).
    pub target_env: &'a [(String, String)],
    pub target_inherited: &'a [String],
    pub executable: &'a str,
    pub binary_args: &'a [String],
    pub residue: &'a [String],
    /// The runfiles working directory (`<tree>/_main`), if any.
    pub runfiles_working_directory: Option<&'a str>,
    /// The built `--run_under` target's executable (LabelRunUnder), if any.
    pub run_under_executable: Option<&'a str>,
}

/// RunCommand.getCommandLineInfo/constructCommandLine (:688-752, 862-912).
pub fn command_line(inputs: &RunInputs<'_>) -> RunCommandLine {
    // makeMutableRunEnvironment (:853-860).
    let mut environment = BTreeMap::from([
        (
            "BUILD_WORKSPACE_DIRECTORY".to_owned(),
            inputs.workspace.to_owned(),
        ),
        (
            "BUILD_WORKING_DIRECTORY".to_owned(),
            inputs.working_directory.to_owned(),
        ),
        ("BUILD_EXECROOT".to_owned(), inputs.execroot.to_owned()),
        ("BUILD_ID".to_owned(), inputs.build_id.to_owned()),
    ]);
    let mut clear = BTreeSet::new();
    for variable in &inputs.options.run_env {
        match variable {
            EnvVar::Set(name, value) => {
                environment.insert(name.clone(), value.clone());
                clear.remove(name);
            }
            EnvVar::Inherit(name) => {
                match inputs.client_env.get(name) {
                    Some(value) => environment.insert(name.clone(), value.clone()),
                    None => environment.remove(name),
                };
                clear.remove(name);
            }
            EnvVar::Unset(name) => {
                environment.remove(name);
                clear.insert(name.clone());
            }
        }
    }
    // ActionEnvironment.resolve (actions/ActionEnvironment.java:123-132).
    for (name, value) in inputs.target_env {
        environment.insert(name.clone(), value.clone());
    }
    for name in inputs.target_inherited {
        if let Some(value) = inputs.client_env.get(name) {
            environment.insert(name.clone(), value.clone());
        }
    }
    clear.extend(ENV_TO_CLEAR.iter().map(|name| (*name).to_owned()));
    let working_directory = match inputs.runfiles_working_directory {
        Some(directory) if !inputs.options.run_in_cwd => directory.to_owned(),
        _ => inputs.working_directory.to_owned(),
    };
    let mut args = vec![inputs.executable.to_owned()];
    args.extend(inputs.binary_args.iter().cloned());
    args.extend(inputs.residue.iter().cloned());
    RunCommandLine {
        args,
        residue: inputs.residue.len(),
        // Builder.setRunUnderTarget/setRunUnderPrefix (RunCommandLine.java
        // :354-377): the label's escaped executable path and escaped options,
        // or the command prefix as given.
        run_under: inputs.options.run_under.as_ref().map(|run_under| {
            match inputs.run_under_executable.filter(|_| run_under.is_label()) {
                Some(executable) => std::iter::once(executable)
                    .chain(run_under.options.iter().map(String::as_str))
                    .map(shell_escape)
                    .collect::<Vec<_>>()
                    .join(" "),
                None => run_under.value.clone(),
            }
        }),
        environment,
        clear,
        working_directory,
    }
}

impl RunCommandLine {
    /// The command of LinuxFormatter.formatArgv (RunCommandLine.java:166-182):
    /// the run_under prefix, then the escaped arguments.
    fn command(&self, args: &[String]) -> String {
        let mut command = String::new();
        if let Some(prefix) = &self.run_under {
            command.push_str(prefix);
            command.push(' ');
        }
        command.push_str(
            &args
                .iter()
                .map(|arg| shell_escape(arg))
                .collect::<Vec<_>>()
                .join(" "),
        );
        command
    }

    /// LinuxFormatter.formatArgv: `[shell, -c, command]`.
    pub fn argv(&self, shell: &str) -> Vec<String> {
        vec![shell.to_owned(), "-c".to_owned(), self.command(&self.args)]
    }

    /// getPrettyArgs (RunCommandLine.java:94-117) with the absolute
    /// executable path: residue shown as ` <args omitted>` by default.
    pub fn pretty(&self, omit_run_args: bool) -> String {
        let mut result = String::new();
        if let Some(prefix) = &self.run_under {
            result.push_str(prefix);
            result.push(' ');
        }
        let fixed = self.args.len() - self.residue;
        result.push_str(
            &self.args[..fixed]
                .iter()
                .map(|arg| shell_escape(arg))
                .collect::<Vec<_>>()
                .join(" "),
        );
        if self.residue > 0 {
            if omit_run_args {
                result.push_str(" <args omitted>");
            } else {
                for arg in &self.args[fixed..] {
                    result.push(' ');
                    result.push_str(&shell_escape(arg));
                }
            }
        }
        result
    }

    /// LinuxFormatter.getScriptForm (RunCommandLine.java:184-212): no
    /// trailing newline.
    pub fn script(&self, shell: &str) -> String {
        let unset = self
            .clear
            .iter()
            .map(|name| format!("-u {name}"))
            .collect::<Vec<_>>()
            .join(" \\\n    ");
        let set = self
            .environment
            .iter()
            .map(|(name, value)| format!("{}={}", shell_escape(name), shell_escape(value)))
            .collect::<Vec<_>>()
            .join(" \\\n    ");
        let command = if self.run_under.is_some() {
            format!("{shell} -c {}", shell_escape(&self.command(&self.args)))
        } else {
            self.command(&self.args)
        };
        format!(
            "#!{shell}\ncd {} && \\\n  exec env \\\n    {unset} \\\n    {set} \\\n  {command} \"$@\"",
            shell_escape(&self.working_directory)
        )
    }
}

/// A random (version 4) UUID, like `CommandEnvironment.getCommandId`.
pub fn random_uuid() -> Result<String, String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("cannot read /dev/urandom: {error}"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(
        options: &'a RunOptions,
        client: &'a BTreeMap<String, String>,
        residue: &'a [String],
    ) -> RunInputs<'a> {
        RunInputs {
            workspace: "/ws",
            working_directory: "/ws/pkg",
            execroot: "/x",
            build_id: "id",
            client_env: client,
            options,
            target_env: &[],
            target_inherited: &[],
            executable: "/x/bazel-out/k8-fastbuild/bin/pkg/b",
            binary_args: &[],
            residue,
            runfiles_working_directory: Some("/x/bazel-out/k8-fastbuild/bin/pkg/b.runfiles/_main"),
            run_under_executable: None,
        }
    }

    #[test]
    fn script_form_matches_the_linux_formatter() {
        let options = RunOptions::default();
        let client = BTreeMap::new();
        let residue = vec!["arg1".to_owned()];
        let line = command_line(&inputs(&options, &client, &residue));
        assert_eq!(
            line.script("/bin/bash"),
            "#!/bin/bash\ncd /x/bazel-out/k8-fastbuild/bin/pkg/b.runfiles/_main && \\\n  exec env \\\n    -u JAVA_RUNFILES \\\n    -u RUNFILES_DIR \\\n    -u RUNFILES_MANIFEST_FILE \\\n    -u RUNFILES_MANIFEST_ONLY \\\n    -u TEST_SRCDIR \\\n    BUILD_EXECROOT=/x \\\n    BUILD_ID=id \\\n    BUILD_WORKING_DIRECTORY=/ws/pkg \\\n    BUILD_WORKSPACE_DIRECTORY=/ws \\\n  /x/bazel-out/k8-fastbuild/bin/pkg/b arg1 \"$@\""
        );
        assert_eq!(
            line.argv("/bin/bash"),
            [
                "/bin/bash",
                "-c",
                "/x/bazel-out/k8-fastbuild/bin/pkg/b arg1"
            ]
        );
        assert_eq!(
            line.pretty(true),
            "/x/bazel-out/k8-fastbuild/bin/pkg/b <args omitted>"
        );
    }

    #[test]
    fn run_under_prefix_wraps_the_script_command() {
        let options = RunOptions {
            run_under: Some(RunUnder::parse("env FOO=1").unwrap()),
            ..RunOptions::default()
        };
        let client = BTreeMap::new();
        let line = command_line(&inputs(&options, &client, &[]));
        assert!(
            line.script("/bin/bash")
                .ends_with("  /bin/bash -c 'env FOO=1 /x/bazel-out/k8-fastbuild/bin/pkg/b' \"$@\"")
        );
        assert_eq!(
            line.argv("/bin/bash")[2],
            "env FOO=1 /x/bazel-out/k8-fastbuild/bin/pkg/b"
        );
    }

    /// RunUnderConverter and TestStrategy.addRunUnderArgs (genuine 9.3
    /// captures, tests/fixtures/cli_test_options).
    #[test]
    fn run_under_forms() {
        let label = RunUnder::parse("//p:runner --flag 'a b'").unwrap();
        assert!(label.is_label() && !label.needs_shell());
        assert_eq!(label.options, ["--flag", "a b"]);
        assert_eq!(
            label.test_prefix(Some("p/runner.sh"), "/bin/bash"),
            ["p/runner.sh", "--flag", "a b"]
        );
        let shell = RunUnder::parse("echo pre").unwrap();
        assert_eq!(
            shell.test_prefix(None, "/bin/bash"),
            ["/bin/bash", "-c", "\"$@\"", "/bin/bash", "echo", "pre"]
        );
        let direct = RunUnder::parse("/usr/bin/env FOO=1").unwrap();
        assert_eq!(
            direct.test_prefix(None, "/bin/bash"),
            ["/usr/bin/env", "FOO=1"]
        );
        assert_eq!(RunUnder::parse("  ").unwrap_err(), "Empty command");
        assert!(
            RunUnder::parse("'x")
                .unwrap_err()
                .starts_with("Not a valid command prefix ")
        );
        let options = RunOptions {
            run_under: Some(label),
            ..RunOptions::default()
        };
        let client = BTreeMap::new();
        let mut inputs = inputs(&options, &client, &[]);
        inputs.run_under_executable = Some("/x/bazel-out/k8-fastbuild/bin/p/runner.sh");
        assert_eq!(
            command_line(&inputs).argv("/bin/bash")[2],
            "/x/bazel-out/k8-fastbuild/bin/p/runner.sh --flag 'a b' /x/bazel-out/k8-fastbuild/bin/pkg/b"
        );
    }

    #[test]
    fn run_env_then_target_environment() {
        let options = RunOptions {
            run_env: vec![
                EnvVar::parse("A=1").unwrap(),
                EnvVar::parse("=B").unwrap(),
                EnvVar::parse("C").unwrap(),
                EnvVar::parse("D").unwrap(),
            ],
            ..RunOptions::default()
        };
        let client = BTreeMap::from([
            ("C".to_owned(), "c".to_owned()),
            ("I".to_owned(), "i".to_owned()),
        ]);
        let mut run = inputs(&options, &client, &[]);
        let env = [("A".to_owned(), "target".to_owned())];
        let inherited = ["I".to_owned(), "MISSING".to_owned()];
        run.target_env = &env;
        run.target_inherited = &inherited;
        let line = command_line(&run);
        assert_eq!(line.environment["A"], "target");
        assert_eq!(line.environment["C"], "c");
        assert_eq!(line.environment["I"], "i");
        assert!(!line.environment.contains_key("D") && !line.environment.contains_key("MISSING"));
        assert!(line.clear.contains("B") && line.clear.contains("TEST_SRCDIR"));
        assert_eq!(shell_escape("x y'z"), "'x y'\\''z'");
        assert_eq!(shell_escape(""), "''");
        assert_eq!(shell_escape("~a"), "'~a'");
        assert_eq!(shell_escape("a~"), "a~");
        assert!(random_uuid().unwrap().len() == 36);
    }
}
