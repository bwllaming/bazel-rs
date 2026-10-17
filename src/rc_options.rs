//! Command options that real `.bazelrc` files set and that the native
//! profile models outside the build configuration (N40, docs/RC_OPTIONS.md).
//! Pinned source: `/tmp/opencode/bazel-9.3-reference`; paths are relative to
//! `src/main/java/com/google/devtools/build/lib/`.
//!
//! Each option either has its Bazel effect natively, is accepted because
//! Bazel gives it no effect in what the native profile can run (cited per
//! option), or fails explicitly. Values of `--repo_env` are never printed.

use crate::repo_env::EnvVar;

/// The values of the options this module models, in priority order.
#[derive(Clone, Debug)]
pub struct RcOptions {
    /// `--repo_env` values (CommonCommandOptions.java:607-622), in order.
    pub repo_env: Vec<EnvVar>,
    /// `--attempt_to_print_relative_paths` (UiOptions.java:221-229).
    pub attempt_to_print_relative_paths: bool,
    /// `--experimental_isolated_extension_usages`
    /// (packages/semantics/BuildLanguageOptions.java:278-286).
    pub isolated_extension_usages: bool,
    /// `--remote_upload_local_results` (remote/options/RemoteOptions.java:333-340,
    /// default true). Only consulted with a disk, HTTP or gRPC cache or remote
    /// execution (remote/RemoteModule.java:526-531); none of them is a native
    /// option, so the value has no effect yet (the remote cache work reads it).
    pub remote_upload_local_results: bool,
    /// `--test_output` (exec/ExecutionOptions.java:271-293); only test
    /// execution reads it (runtime/commands/TestCommand.java:84-100,
    /// runtime/TestSummaryPrinter.java:51-90), so it has no effect on `build`.
    pub test_output: &'static str,
    /// `--test_summary` (exec/ExecutionOptions.java:311-330); test-only like
    /// `--test_output`.
    pub test_summary: &'static str,
}

impl Default for RcOptions {
    fn default() -> Self {
        Self {
            repo_env: Vec::new(),
            attempt_to_print_relative_paths: false,
            isolated_extension_usages: false,
            remote_upload_local_results: true,
            test_output: "summary",
            test_summary: "short",
        }
    }
}

/// ExecutionOptions.TestOutputFormat (:528-549), EnumConverter names.
const TEST_OUTPUT: &[&str] = &["summary", "errors", "all", "streamed"];
/// ExecutionOptions.TestSummaryFormat (:552-577).
const TEST_SUMMARY: &[&str] = &[
    "short",
    "short_uncached",
    "terse",
    "detailed",
    "detailed_uncached",
    "none",
    "testcase",
];

/// Remote execution options (remote/options/RemoteOptions.java:91, :367, :601).
const REMOTE_EXECUTION: &[&str] = &[
    "remote_executor",
    "remote_instance_name",
    "remote_default_exec_properties",
];

/// `ARG :: MESSAGE` (BlazeOptionHandler.java:104, 628-637).
fn invalid_argument(arg: &str, message: String) -> String {
    format!("{arg} :: {message}")
}

/// An option that takes a value: `--NAME=VALUE` or `--NAME VALUE`; the
/// `no` prefix is illegal. `(form, value, used_next)`.
fn value_option<'a>(
    arg: &'a str,
    next: Option<&'a str>,
    name: &str,
) -> Option<Result<(String, &'a str, bool), String>> {
    let rest = arg.strip_prefix("--")?;
    let (given, value) = match rest.split_once('=') {
        Some((given, value)) => (given, Some(value)),
        None => (rest, None),
    };
    if given == name {
        return Some(match value {
            Some(value) => Ok((arg.to_owned(), value, false)),
            None => match next {
                Some(value) => Ok((format!("{arg} {value}"), value, true)),
                None => Err(format!("Expected value after {arg}")),
            },
        });
    }
    if given.strip_prefix("no") == Some(name) {
        return Some(Err(invalid_argument(
            arg,
            format!("Illegal use of 'no' prefix on non-boolean option: {arg}"),
        )));
    }
    None
}

/// A boolean option under its name or old name.
fn boolean(arg: &str, names: &[&str]) -> Option<Result<bool, String>> {
    names
        .iter()
        .find_map(|name| crate::configuration::boolean_option(arg, name))
}

/// EnumConverter (common/options/EnumConverter.java:32-67).
fn enumeration(
    form: &str,
    value: &str,
    type_name: &str,
    values: &'static [&'static str],
) -> Result<&'static str, String> {
    values
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(value))
        .copied()
        .ok_or_else(|| {
            let mut list = String::new();
            for (index, candidate) in values.iter().enumerate() {
                if index > 0 {
                    list.push_str(if index + 1 < values.len() {
                        ", "
                    } else {
                        " or "
                    });
                }
                list.push_str(candidate);
            }
            format!(
                "While parsing option {form}: Not a valid {type_name}: '{value}' (should be {list})"
            )
        })
}

impl RcOptions {
    /// Parse one option of `command` (`build`, `query` or `mod`). `Ok(None)`
    /// when `arg` is not one of these options; `Ok(Some(used_next))` when it
    /// was consumed. `origin` is the rc file:line suffix for explicit errors.
    pub fn parse(
        &mut self,
        command: &str,
        arg: &str,
        next: Option<&str>,
        origin: &str,
    ) -> Result<Option<bool>, String> {
        let build = command == "build";
        let unrecognized = || invalid_argument(arg, format!("Unrecognized option: {arg}"));
        if let Some(parsed) = value_option(arg, next, "repo_env") {
            let (form, value, used) = parsed?;
            // The only invalid inputs ("" and "=") carry no value.
            let value = crate::repo_env::convert(value)
                .map_err(|error| format!("While parsing option {form}: {error}"))?;
            self.repo_env.push(value);
            return Ok(Some(used));
        }
        if let Some(value) = boolean(
            arg,
            &[
                "attempt_to_print_relative_paths",
                "experimental_ui_attempt_to_print_relative_paths",
            ],
        ) {
            self.attempt_to_print_relative_paths = value?;
            return Ok(Some(false));
        }
        if let Some(value) = boolean(arg, &["experimental_isolated_extension_usages"]) {
            self.isolated_extension_usages = value?;
            return Ok(Some(false));
        }
        if let Some(value) = boolean(arg, &["remote_upload_local_results"]) {
            self.remote_upload_local_results = value?;
            return Ok(Some(false));
        }
        if let Some(value) = boolean(arg, &["incompatible_enable_proto_toolchain_resolution"]) {
            // Default true (packages/semantics/FlagConstants.java:48); the
            // native proto_common_do_not_use exports True (crate::language_modules).
            if !value? {
                return Err(format!(
                    "unsupported option {arg}{origin}: --incompatible_enable_proto_toolchain_resolution=false (the legacy --proto_compiler/--proto_toolchain_for_* path of proto rules) is not implemented; only the Bazel 9.3 default true is modelled"
                ));
            }
            return Ok(Some(false));
        }
        if let Some(value) = boolean(
            arg,
            &[
                "incompatible_strict_action_env",
                "experimental_strict_action_env",
            ],
        ) {
            if !build {
                return Err(unrecognized());
            }
            // Default true (bazel/rules/BazelRuleClassProvider.java:73-87); the
            // strict default shell environment is PATH=/bin:/usr/bin:/usr/local/bin
            // (:140-200, :347-360).
            if !value? {
                return Err(format!(
                    "unsupported option {arg}{origin}: a non-strict action environment (PATH and LD_LIBRARY_PATH inherited from the client) is not implemented; only the Bazel 9.3 default --incompatible_strict_action_env=true is modelled"
                ));
            }
            return Ok(Some(false));
        }
        for (name, type_name, values) in [
            ("test_output", "test output", TEST_OUTPUT),
            ("test_summary", "test summary", TEST_SUMMARY),
        ] {
            if let Some(parsed) = value_option(arg, next, name) {
                if !build {
                    return Err(unrecognized());
                }
                let (form, value, used) = parsed?;
                let value = enumeration(&form, value, type_name, values)?;
                if name == "test_output" {
                    self.test_output = value;
                } else {
                    self.test_summary = value;
                }
                return Ok(Some(used));
            }
        }
        if let Some(parsed) = value_option(arg, next, "sandbox_add_mount_pair") {
            if !build {
                return Err(unrecognized());
            }
            parsed?;
            // SandboxOptions.java:173-180; linux-sandbox bind mounts. Bazel's
            // default Linux strategy sandboxes, so this is not a no-op there.
            return Err(format!(
                "unsupported option --sandbox_add_mount_pair{origin}: sandboxed execution is not implemented (native actions run unsandboxed in a private execroot)"
            ));
        }
        for name in REMOTE_EXECUTION {
            if let Some(parsed) = value_option(arg, next, name) {
                parsed?;
                return Err(format!(
                    "unsupported option --{name}{origin}: remote execution is not implemented"
                ));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(command: &str, args: &[&str]) -> Result<RcOptions, String> {
        let mut options = RcOptions::default();
        let mut index = 0;
        while index < args.len() {
            match options.parse(command, args[index], args.get(index + 1).copied(), "")? {
                None => panic!("not modelled: {}", args[index]),
                Some(next) => index += 1 + usize::from(next),
            }
        }
        Ok(options)
    }

    #[test]
    fn code_rc_options_parse() {
        let options = parse(
            "build",
            &[
                "--incompatible_strict_action_env",
                "--test_output=errors",
                "--test_summary",
                "TERSE",
                "--attempt_to_print_relative_paths=true",
                "--repo_env=JAVA_HOME=../bazel_tools/jdk",
                "--noremote_upload_local_results",
                "--experimental_isolated_extension_usages",
                "--incompatible_enable_proto_toolchain_resolution",
                "--repo_env",
                "NO_PROXY=x",
            ],
        )
        .unwrap();
        assert!(options.attempt_to_print_relative_paths);
        assert!(options.isolated_extension_usages);
        assert!(!options.remote_upload_local_results);
        assert_eq!(
            (options.test_output, options.test_summary),
            ("errors", "terse")
        );
        assert_eq!(
            options.repo_env,
            [
                EnvVar::Set("JAVA_HOME".into(), "../bazel_tools/jdk".into()),
                EnvVar::Set("NO_PROXY".into(), "x".into())
            ]
        );
        let query = parse(
            "query",
            &[
                "--experimental_ui_attempt_to_print_relative_paths",
                "--repo_env==X",
            ],
        )
        .unwrap();
        assert!(query.attempt_to_print_relative_paths);
    }

    #[test]
    fn errors_follow_bazel_or_are_explicit() {
        let error = |command: &str, args: &[&str]| parse(command, args).unwrap_err();
        assert_eq!(
            error("build", &["--repo_env="]),
            "While parsing option --repo_env=: Variable definitions must be in the form of a 'name=value', 'name', or '=name' assignment"
        );
        assert_eq!(
            error("build", &["--repo_env"]),
            "Expected value after --repo_env"
        );
        assert_eq!(
            error("build", &["--norepo_env"]),
            "--norepo_env :: Illegal use of 'no' prefix on non-boolean option: --norepo_env"
        );
        assert_eq!(
            error("build", &["--test_output=some"]),
            "While parsing option --test_output=some: Not a valid test output: 'some' (should be summary, errors, all or streamed)"
        );
        assert_eq!(
            error("query", &["--test_output=errors"]),
            "--test_output=errors :: Unrecognized option: --test_output=errors"
        );
        assert_eq!(
            error("build", &["--attempt_to_print_relative_paths=maybe"]),
            "While parsing option --attempt_to_print_relative_paths=maybe: 'maybe' is not a boolean"
        );
        for (args, needle) in [
            (
                &["--noincompatible_strict_action_env"][..],
                "non-strict action environment",
            ),
            (
                &["--incompatible_enable_proto_toolchain_resolution=false"][..],
                "legacy --proto_compiler",
            ),
            (
                &["--sandbox_add_mount_pair=/tmp"][..],
                "sandboxed execution is not implemented",
            ),
            (
                &["--remote_executor=grpc://h:1"][..],
                "remote execution is not implemented",
            ),
            (
                &["--remote_instance_name", "dev"][..],
                "remote execution is not implemented",
            ),
        ] {
            let message = error("build", args);
            assert!(message.contains(needle), "{message}");
            assert!(!message.contains("grpc://"), "{message}");
        }
    }
}
