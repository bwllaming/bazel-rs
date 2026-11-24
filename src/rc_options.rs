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
#[derive(Clone, Debug, Default)]
pub struct RcOptions {
    /// `--repo_env` values (CommonCommandOptions.java:607-622), in order.
    pub repo_env: Vec<EnvVar>,
    /// `--attempt_to_print_relative_paths` (UiOptions.java:221-229).
    pub attempt_to_print_relative_paths: bool,
    /// `--experimental_isolated_extension_usages`
    /// (packages/semantics/BuildLanguageOptions.java:278-286).
    pub isolated_extension_usages: bool,
    /// The garbage collection options of the native cache
    /// (`--experimental_disk_cache_gc_*`, `--repo_contents_cache_gc_*`;
    /// crate::cache_gc, docs/CACHE_GC.md).
    pub gc: crate::cache_gc::GcOptions,
    /// `--repository_cache` (old name `--experimental_repository_cache`;
    /// RepositoryOptions.java:40-54) as given: `None` unset, `""`
    /// disabled (crate::repository_cache, docs/REPOSITORY_CACHE.md).
    pub repository_cache: Option<String>,
    /// `--repo_contents_cache` (old name `--repository_contents_cache`;
    /// RepositoryOptions.java:56-72) as given.
    pub repo_contents_cache: Option<String>,
    /// `--experimental_repository_cache_hardlinks`
    /// (RepositoryOptions.java:155-163): accepted, cache hits are copied
    /// (crate::repository_cache::materialize_hit).
    pub repository_cache_hardlinks: bool,
    /// `--experimental_repository_downloader_retries`
    /// (RepositoryOptions.java:179-188; default 5).
    pub repository_downloader_retries: Option<i32>,
    /// `--http_connector_attempts` (CommonCommandOptions.java:675-681;
    /// default 8): crate::download_options.
    pub http_connector_attempts: Option<i32>,
    /// `--http_timeout_scaling` (CommonCommandOptions.java:667-673;
    /// default 1.0): crate::download_options.
    pub http_timeout_scaling: Option<f64>,
    /// `--downloader_config` (old name `--experimental_downloader_config`;
    /// RepositoryOptions.java:278-291), the last value: crate::download_options.
    pub downloader_config: Option<String>,
    /// `--distdir` (old name `--experimental_distdir`; RepositoryOptions.java
    /// :190-201, allowMultiple), in order: crate::download_options.
    pub distdir: Vec<String>,
    /// `--module_mirrors` (RepositoryOptions.java:112-133, allowMultiple
    /// `[registry=]mirror,…` assignments), in order: crate::download_options.
    pub module_mirrors: Vec<(String, Vec<String>)>,
    /// Old option names used (OptionsParserImpl.maybeAddOldNameWarning).
    pub warnings: Vec<String>,
    /// `--incompatible_enforce_starlark_utf8` (`OFF`, `WARNING`, `ERROR`),
    /// for `info starlark-semantics` only.
    pub starlark_utf8: Option<&'static str>,
    /// `--incompatible_use_cc_configure_from_rules_cc`, for `info
    /// starlark-semantics` only (no other effect; see [`utf8_enforcement`]'s
    /// neighbours).
    pub use_cc_configure_from_rules_cc: bool,
    /// `--[no]incompatible_enable_proto_toolchain_resolution`
    /// (BuildLanguageOptions.java:644-652; `None`: the default true).
    pub proto_toolchain_resolution: Option<bool>,
}

/// `--NAME`'s value through Converters.IntegerConverter.
fn int_value(form: &str, value: &str) -> Result<i32, String> {
    value
        .parse::<i32>()
        .map_err(|_| format!("While parsing option {form}: '{value}' is not an int"))
}

/// Converters.AssignmentToListOfValuesConverter with empty keys allowed
/// (StringToStringListConverter, Converters.java:525-597).
pub fn string_to_string_list(value: &str) -> Result<(String, Vec<String>), String> {
    let (key, values) = match value.find('=') {
        Some(at) if at > 0 => (&value[..at], &value[at + 1..]),
        Some(at) => ("", &value[at + 1..]),
        None => ("", value),
    };
    let values: Vec<String> = values.split(',').map(str::to_owned).collect();
    if values.iter().any(String::is_empty) {
        if values.len() == 1 {
            return Ok((key.to_owned(), Vec::new()));
        }
        return Err(
            "Variable definitions must not contain empty strings or leading / trailing commas"
                .to_owned(),
        );
    }
    Ok((key.to_owned(), values))
}

/// `ARG :: MESSAGE` (BlazeOptionHandler.java:104, 628-637).
fn invalid_argument(arg: &str, message: String) -> String {
    format!("{arg} :: {message}")
}

/// An option that takes a value: `--NAME=VALUE` or `--NAME VALUE`; the
/// `no` prefix is illegal. `(form, value, used_next)`.
pub fn value_option<'a>(
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

/// One of the garbage collection options (crate::cache_gc::OPTION_NAMES),
/// which every command takes (genuine: the remote and repository modules'
/// common options): `Ok(Some(used_next))` when `arg` is one.
pub fn gc_option(
    gc: &mut crate::cache_gc::GcOptions,
    arg: &str,
    next: Option<&str>,
) -> Result<Option<bool>, String> {
    for name in crate::cache_gc::OPTION_NAMES {
        if let Some(parsed) = value_option(arg, next, name) {
            let (form, value, used) = parsed?;
            gc.set(name, value)
                .map_err(|error| format!("While parsing option {form}: {error}"))?;
            return Ok(Some(used));
        }
    }
    Ok(None)
}

/// A boolean option under its name or old name.
fn boolean(arg: &str, names: &[&str]) -> Option<Result<bool, String>> {
    names
        .iter()
        .find_map(|name| crate::configuration::boolean_option(arg, name))
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
        // `run` and `test` inherit build's options (N23f, N23g).
        let build = matches!(command, "build" | "run" | "test");
        let unrecognized = || invalid_argument(arg, format!("Unrecognized option: {arg}"));
        if let Some(used) = gc_option(&mut self.gc, arg, next)? {
            return Ok(Some(used));
        }
        // RepositoryOptions (every command): the repository cache options
        // (crate::repository_cache, docs/REPOSITORY_CACHE.md).
        for (names, slot) in [
            (
                ["repository_cache", "experimental_repository_cache"],
                &mut self.repository_cache,
            ),
            (
                ["repo_contents_cache", "repository_contents_cache"],
                &mut self.repo_contents_cache,
            ),
        ] {
            for name in names {
                if let Some(parsed) = value_option(arg, next, name) {
                    let (_, value, used) = parsed?;
                    *slot = Some(value.to_owned());
                    return Ok(Some(used));
                }
            }
        }
        if let Some(value) = boolean(arg, &["experimental_repository_cache_hardlinks"]) {
            self.repository_cache_hardlinks = value?;
            return Ok(Some(false));
        }
        // RemoteOptions.remoteDownloader (:145-155): the Remote Asset API
        // download proxy changes where every download comes from; only its
        // default (empty) is modelled. Its two booleans are consulted only
        // with a downloader.
        if let Some(parsed) = value_option(arg, next, "experimental_remote_downloader") {
            let (form, value, used) = parsed?;
            if !value.is_empty() {
                return Err(format!(
                    "unsupported option {form:?}{origin}: the Remote Asset API downloader is not implemented by the native profile; options are never silently ignored"
                ));
            }
            return Ok(Some(used));
        }
        if let Some(value) = boolean(
            arg,
            &[
                "experimental_remote_downloader_local_fallback",
                "experimental_remote_downloader_propagate_credentials",
            ],
        ) {
            value?;
            return Ok(Some(false));
        }
        if let Some(parsed) = value_option(arg, next, "experimental_repository_downloader_retries")
        {
            let (form, value, used) = parsed?;
            // Converters.IntegerConverter.
            let retries = value
                .parse::<i32>()
                .map_err(|_| format!("While parsing option {form}: '{value}' is not an int"))?;
            self.repository_downloader_retries = Some(retries);
            return Ok(Some(used));
        }
        // CommonCommandOptions' HTTP downloader settings (every command).
        if let Some(parsed) = value_option(arg, next, "http_connector_attempts") {
            let (form, value, used) = parsed?;
            self.http_connector_attempts = Some(int_value(&form, value)?);
            return Ok(Some(used));
        }
        if let Some(parsed) = value_option(arg, next, "http_timeout_scaling") {
            let (form, value, used) = parsed?;
            // Converters.DoubleConverter (Double.parseDouble).
            let scaling = crate::download_options::parse_java_double(value)
                .ok_or_else(|| format!("While parsing option {form}: '{value}' is not a double"))?;
            self.http_timeout_scaling = Some(scaling);
            return Ok(Some(used));
        }
        // RepositoryOptions (every command): the downloader's URL rewriter,
        // distribution directories and module mirrors.
        for (name, old) in [
            ("downloader_config", "experimental_downloader_config"),
            ("distdir", "experimental_distdir"),
        ] {
            for given in [name, old] {
                if let Some(parsed) = value_option(arg, next, given) {
                    let (_, value, used) = parsed?;
                    if given == old {
                        let warning = format!("Option '{old}' is deprecated: Use --{name} instead");
                        if !self.warnings.contains(&warning) {
                            self.warnings.push(warning);
                        }
                    }
                    if name == "distdir" {
                        self.distdir.push(value.to_owned());
                    } else {
                        // PathFragmentConverter: the empty value is unset.
                        self.downloader_config = (!value.is_empty()).then(|| value.to_owned());
                    }
                    return Ok(Some(used));
                }
            }
        }
        if let Some(parsed) = value_option(arg, next, "module_mirrors") {
            let (form, value, used) = parsed?;
            let entry = string_to_string_list(value)
                .map_err(|error| format!("While parsing option {form}: {error}"))?;
            self.module_mirrors.push(entry);
            return Ok(Some(used));
        }
        // UiOptions.terminalColumns (:166-173): a client-supplied width for
        // the curses progress bar; the native display measures the terminal
        // itself. Converted, no effect (docs/RC_OPTIONS.md).
        if let Some(parsed) = value_option(arg, next, "terminal_columns") {
            let (form, value, used) = parsed?;
            int_value(&form, value)?;
            return Ok(Some(used));
        }
        // AuthAndTLSOptions.grpcKeepaliveTime (:117-128): keep-alive pings of
        // gRPC connections with a pending call. Converted, no effect: the
        // native gRPC channels (remote cache and execution, BES) keep their
        // own HTTP/2 keep-alive settings (docs/RC_OPTIONS.md).
        if let Some(parsed) = value_option(arg, next, "grpc_keepalive_time") {
            let (form, value, used) = parsed?;
            crate::cache_gc::parse_duration(value)
                .map_err(|error| format!("While parsing option {form}: {error}"))?;
            return Ok(Some(used));
        }
        // BuildLanguageOptions.incompatibleEnforceStarlarkUtf8 (:812-842,
        // BoolOrEnumConverter: true is `error`, false `off`). The native
        // loader always fails a Starlark file that is not UTF-8 (the `error`
        // mode, with its own message); `warning` (the default) and `off`
        // decode such a file as Latin-1 in Bazel, which the native loader
        // does not (docs/RC_OPTIONS.md).
        if let Some(result) = utf8_enforcement(arg) {
            self.starlark_utf8 = Some(result?);
            return Ok(Some(false));
        }
        // BuildLanguageOptions.incompatibleUseCcConfigureFromRulesCc
        // (:534-544): its only consumer is the hidden `__do_not_use_fail_
        // with_incompatible_use_cc_configure_from_rules_cc` builtin
        // (StarlarkRepositoryModule.java:195-207), which Bazel 9.3's
        // @bazel_tools no longer calls (cc_configure moved to rules_cc) and
        // the native profile does not define: no effect beyond `info
        // starlark-semantics`.
        if let Some(value) = boolean(arg, &["incompatible_use_cc_configure_from_rules_cc"]) {
            self.use_cc_configure_from_rules_cc = value?;
            return Ok(Some(false));
        }
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
        // `--[no]remote_upload_local_results`, `--remote_instance_name`,
        // `--remote_executor`, `--remote_default_exec_properties` and the
        // other modelled RemoteOptions are crate::remote_cache::RemoteOptions
        // (N41, docs/REMOTE_EXECUTION.md).
        if let Some(value) = boolean(arg, &["incompatible_enable_proto_toolchain_resolution"]) {
            // Default true (packages/semantics/FlagConstants.java:48):
            // `proto_common_do_not_use.INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION`
            // (crate::language_modules). protobuf's rules then take protoc and
            // the lang toolchains from their legacy `configuration_field("proto",
            // …)` attributes instead of toolchain resolution (crate::late_bound).
            self.proto_toolchain_resolution = Some(value?);
            return Ok(Some(false));
        }
        // `--[no]incompatible_strict_action_env` is a configuration option
        // since N42 (crate::configuration, docs/ACTION_ENV.md): both values
        // are modelled there, so it is not claimed here.
        // `--test_output`/`--test_summary` (exec/ExecutionOptions.java:271-330)
        // are N23g's test options (crate::test_command::TestOptions) of the
        // building commands; other commands do not have them.
        for name in ["test_output", "test_summary"] {
            if value_option(arg, next, name).is_some() {
                if !build {
                    return Err(unrecognized());
                }
                return Ok(None);
            }
        }
        // `--sandbox_add_mount_pair` (SandboxOptions.java:173-180) is a
        // SandboxModule option of the building commands only; their native
        // parser models it with the other sandbox options (crate::sandbox).
        if value_option(arg, next, "sandbox_add_mount_pair").is_some() {
            if !build {
                return Err(unrecognized());
            }
            return Ok(None);
        }
        Ok(None)
    }
}

/// `--[no]incompatible_enforce_starlark_utf8[=MODE]`: the mode when `arg`
/// is one.
fn utf8_enforcement(arg: &str) -> Option<Result<&'static str, String>> {
    const NAME: &str = "incompatible_enforce_starlark_utf8";
    let rest = arg.strip_prefix("--")?;
    if rest == NAME {
        return Some(Ok("ERROR"));
    }
    if rest == format!("no{NAME}") {
        return Some(Ok("OFF"));
    }
    if rest
        .strip_prefix("no")
        .and_then(|rest| rest.strip_prefix(NAME))
        .is_some_and(|rest| rest.starts_with('='))
    {
        return Some(Err(format!(
            "{arg} :: Unexpected value after boolean option: {arg}"
        )));
    }
    let value = rest.strip_prefix(NAME)?.strip_prefix('=')?;
    Some(match value.to_ascii_lowercase().as_str() {
        "off" => Ok("OFF"),
        "warning" => Ok("WARNING"),
        "error" => Ok("ERROR"),
        "true" | "yes" | "1" => Ok("ERROR"),
        "false" | "no" | "0" => Ok("OFF"),
        _ => Err(format!(
            "While parsing option {arg}: Not a valid UTF-8 enforcement mode: '{value}' (should be off, warning or error)"
        )),
    })
}

/// Boolean options whose Bazel effect does not exist in a native command
/// (docs/RC_OPTIONS.md, "Accepted without effect"): every value is accepted.
const NATIVE_NO_EFFECT_BOOLEANS: &[(&str, &str)] = &[
    // RepositoryOptions.checkExternalRepositoryFiles (:321-331): false skips
    // Bazel's check of external repository files for changes between
    // commands of one server. Native commands verify fetched repositories
    // by their own tamper check (crate::repository_store), which a speed
    // option need not weaken: the result is the same unless repository
    // files are edited by hand, which Bazel then misses.
    (
        "experimental_check_external_repository_files",
        "bazel/repository/RepositoryOptions.java:321-331",
    ),
    // BuildRequestOptions.mergedSkyframeAnalysisExecution (:405-415):
    // whether Skyframe interleaves analysis and execution (Skymeld). The
    // native engine's scheduling is its own; the build's results are the
    // same either way.
    (
        "experimental_merged_skyframe_analysis_execution",
        "buildtool/BuildRequestOptions.java:405-415",
    ),
    // PackageOptions.checkOutputFiles (:181-191): false skips Bazel's check
    // of the previous build's output files for outside modifications
    // between commands of one server (rules_zig's CI .bazelrc). Native
    // commands always verify outputs against the action cache (a modified
    // output re-executes its action, crate::action); a speed option need
    // not weaken that: the result is the same unless outputs are edited by
    // hand, which Bazel then misses.
    (
        "experimental_check_output_files",
        "pkgcache/PackageOptions.java:181-191",
    ),
];

/// Boolean options Bazel 9.3 itself declares as no-ops (`help = "No-op"` or
/// the `NO_OP` effect tag; generated from the pinned sources, paths below
/// `src/main/java/com/google/devtools/build/lib/`): every value is accepted
/// and has no effect, as in Bazel. Only the converter is checked.
#[rustfmt::skip]
const BAZEL_NO_OP_BOOLEANS: &[(&str, &str)] = &[
    ("action_cache_store_output_metadata", "bazel/rules/BazelRulesModule.java"),
    ("announce", "bazel/rules/BazelRulesModule.java"),
    ("bes_best_effort", "bazel/rules/BazelRulesModule.java"),
    ("check_fileset_dependencies_recursively", "bazel/rules/BazelRulesModule.java"),
    ("collapse_duplicate_defines", "bazel/rules/BazelRulesModule.java"),
    ("defer_param_files", "bazel/rules/BazelRulesModule.java"),
    ("disallow_strict_deps_for_jpl", "bazel/rules/BazelRulesModule.java"),
    ("discard_actions_after_execution", "bazel/rules/BazelRulesModule.java"),
    ("enable_bzlmod", "bazel/rules/BazelRulesModule.java"),
    ("enable_fdo_profile_absolute_path", "bazel/rules/BazelRulesModule.java"),
    ("enable_workspace", "bazel/rules/BazelRulesModule.java"),
    ("experimental_action_resource_set", "bazel/rules/BazelRulesModule.java"),
    ("experimental_allow_android_library_deps_without_srcs", "rules/android/AndroidConfiguration.java"),
    ("experimental_allow_runtime_deps_on_neverlink", "bazel/rules/BazelRulesModule.java"),
    ("experimental_announce_profile_path", "bazel/rules/BazelRulesModule.java"),
    ("experimental_build_transitive_python_runfiles", "bazel/rules/BazelRulesModule.java"),
    ("experimental_cc_static_library", "bazel/rules/BazelRulesModule.java"),
    ("experimental_collect_local_action_metrics", "bazel/rules/BazelRulesModule.java"),
    ("experimental_collect_local_sandbox_action_metrics", "bazel/rules/BazelRulesModule.java"),
    ("experimental_disallow_legacy_java_toolchain_flags", "bazel/rules/BazelRulesModule.java"),
    ("experimental_enable_starlark_doc_extract", "bazel/rules/BazelRulesModule.java"),
    ("experimental_java_header_compilation_direct_deps", "bazel/rules/BazelRulesModule.java"),
    ("experimental_java_header_input_pruning", "bazel/rules/BazelRulesModule.java"),
    ("experimental_java_proto_add_allowed_public_imports", "bazel/rules/BazelRulesModule.java"),
    ("experimental_limit_android_lint_to_android_constrained_java", "bazel/rules/BazelRulesModule.java"),
    ("experimental_objc_provider_from_linked", "rules/apple/AppleCommandLineOptions.java"),
    ("experimental_parallel_aquery_output", "bazel/rules/BazelRulesModule.java"),
    ("experimental_parse_headers_skipped_if_corresponding_srcs_found", "bazel/rules/BazelRulesModule.java"),
    ("experimental_platform_cc_test", "bazel/rules/BazelRulesModule.java"),
    ("experimental_post_profile_started_event", "bazel/rules/BazelRulesModule.java"),
    ("experimental_profile_action_counts", "bazel/rules/BazelRulesModule.java"),
    ("experimental_proto_extra_actions", "bazel/rules/BazelRulesModule.java"),
    ("experimental_remote_execution_keepalive", "remote/options/RemoteOptions.java"),
    ("experimental_require_availability_info", "bazel/rules/BazelRulesModule.java"),
    ("experimental_sandboxfs_map_symlink_targets", "sandbox/SandboxOptions.java"),
    ("experimental_show_artifacts", "bazel/rules/BazelRulesModule.java"),
    ("experimental_skip_ttvs_for_genquery", "bazel/rules/BazelRulesModule.java"),
    ("experimental_skyframe_error_handling_refactor", "buildtool/BuildRequestOptions.java"),
    ("experimental_skyframe_native_filesets", "bazel/rules/BazelRulesModule.java"),
    ("experimental_skyframe_prepare_analysis", "bazel/rules/BazelRulesModule.java"),
    ("experimental_starlark_cc_import", "bazel/rules/BazelRulesModule.java"),
    ("experimental_starlark_types", "packages/semantics/BuildLanguageOptions.java"),
    ("experimental_use_event_based_build_completion_status", "bazel/rules/BazelRulesModule.java"),
    ("experimental_use_new_worker_pool", "bazel/rules/BazelRulesModule.java"),
    ("experimental_use_semaphore_for_jobs", "bazel/rules/BazelRulesModule.java"),
    ("experimental_worker_as_resource", "bazel/rules/BazelRulesModule.java"),
    ("fat_apk_hwasan", "rules/android/AndroidConfiguration.java"),
    ("force_ignore_dash_static", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_allow_python_version_transitions", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_auto_configure_host_platform", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_depset_for_java_output_source_jars", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_depset_for_libraries_to_link_getter", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_disable_legacy_cc_provider", "rules/cpp/CppOptions.java"),
    ("incompatible_disable_legacy_flags_cc_toolchain_api", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_disable_native_apple_binary_rule", "rules/objc/ObjcCommandLineOptions.java"),
    ("incompatible_disable_native_repo_rules", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_disable_target_provider_fields", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_disallow_legacy_py_provider", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_disallow_struct_provider_syntax", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_dont_collect_native_libraries_in_data", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_dont_use_javasourceinfoprovider", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_enable_android_toolchain_resolution", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_enable_cc_toolchain_resolution", "rules/cpp/CppOptions.java"),
    ("incompatible_enable_cgo_toolchain_resolution", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_enable_profile_by_default", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_existing_rules_immutable_view", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_legacy_local_fallback", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_load_java_rules_from_bzl", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_load_proto_rules_from_bzl", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_load_python_rules_from_bzl", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_macos_set_install_name", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_make_thinlto_command_lines_standalone", "rules/cpp/CppOptions.java"),
    ("incompatible_new_actions_api", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_no_package_distribs", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_override_toolchain_transition", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_py2_outputs_are_suffixed", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_py3_is_default", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_python_disable_py2", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_remove_binary_profile", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_remove_old_python_version_api", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_require_ctx_in_configure_features", "rules/cpp/CppOptions.java"),
    ("incompatible_require_javaplugininfo_in_javacommon", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_require_linker_input_cc_api", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_top_level_aspects_require_providers", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_use_plus_in_repo_names", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_use_python_toolchains", "bazel/rules/BazelRulesModule.java"),
    ("incompatible_use_toolchain_resolution_for_java_rules", "analysis/PlatformOptions.java"),
    ("incompatible_validate_top_level_header_inclusions", "rules/cpp/CppOptions.java"),
    ("incompatible_visibility_private_attributes_at_definition", "bazel/rules/BazelRulesModule.java"),
    ("j2objc_dead_code_removal", "bazel/rules/BazelRulesModule.java"),
    ("jplPropagateCcLinkParamsStore", "bazel/rules/BazelRulesModule.java"),
    ("legacy_bazel_java_test", "bazel/rules/BazelRulesModule.java"),
    ("legacy_external_runfiles", "bazel/rules/BazelRulesModule.java"),
    ("separate_aspect_deps", "bazel/rules/BazelRulesModule.java"),
    ("strict_deps_java_protos", "bazel/rules/BazelRulesModule.java"),
    ("use_top_level_targets_for_symlinks", "bazel/rules/BazelRulesModule.java"),
    ("verbose_explanations", "bazel/rules/BazelRulesModule.java"),
];

/// Boolean options whose effect is on Bazel's server state, JVM or
/// file-system watching, none of which a native command has (no server,
/// no Skyframe graph kept between commands): accepted with every value.
const SERVER_STATE_BOOLEANS: &[(&str, &str)] = &[
    (
        "discard_analysis_cache",
        "analysis/AnalysisOptions.java:31-38",
    ),
    (
        "track_incremental_state",
        "runtime/CommonCommandOptions.java:582-593",
    ),
    (
        "keep_state_after_build",
        "runtime/CommonCommandOptions.java:594-602",
    ),
    (
        "heap_dump_on_oom",
        "runtime/CommonCommandOptions.java:472-480",
    ),
    ("watchfs", "skyframe/LocalDiffAwareness.java:50-60"),
];

/// Boolean options accepted at their Bazel 9.3 default only (the other
/// value changes behaviour the native profile does not model): `fetch`
/// (PackageOptions.java:169-178; `--nofetch` forbids fetching).
/// `incompatible_disallow_empty_glob` is a modelled Starlark semantics
/// option (crate::module_graph::ResolverOptions::allow_empty_glob).
const DEFAULT_ONLY_BOOLEANS: &[(&str, bool)] = &[("fetch", true)];

/// Whether genuine Bazel registers option `name` for `command`
/// (crate::option_metadata, `help flags-as-proto`).
fn registered_for(name: &str, command: &str) -> bool {
    let Some(bit) = crate::option_metadata::COMMANDS
        .iter()
        .position(|candidate| *candidate == command)
    else {
        return false;
    };
    crate::option_metadata::OPTIONS
        .iter()
        .any(|option| option.name == name && option.commands & (1 << bit) != 0)
}

/// An option this profile accepts because Bazel 9.3 gives it no effect in
/// what a native command runs ([`BAZEL_NO_OP_BOOLEANS`],
/// [`SERVER_STATE_BOOLEANS`], [`DEFAULT_ONLY_BOOLEANS`]): `Some(Ok(false))`
/// when `arg` is one `command` takes, Bazel's `Unrecognized option` when
/// `command` does not register it, the converter's error for a bad value,
/// and an explicit unsupported-option error for a non-default value of a
/// default-only option.
pub fn no_effect_option(command: &str, arg: &str, origin: &str) -> Option<Result<bool, String>> {
    let names = BAZEL_NO_OP_BOOLEANS
        .iter()
        .chain(SERVER_STATE_BOOLEANS)
        .chain(NATIVE_NO_EFFECT_BOOLEANS)
        .map(|(name, _)| (*name, None))
        .chain(
            DEFAULT_ONLY_BOOLEANS
                .iter()
                .map(|(name, default)| (*name, Some(*default))),
        );
    for (name, default) in names {
        let Some(value) = crate::configuration::boolean_option(arg, name) else {
            continue;
        };
        if !registered_for(name, command) {
            return Some(Err(invalid_argument(
                arg,
                format!("Unrecognized option: {arg}"),
            )));
        }
        return Some(match (value, default) {
            (Err(error), _) => Err(error),
            (Ok(value), Some(default)) if value != default => Err(format!(
                "unsupported option {arg}{origin}: only the Bazel 9.3 default --{}{name} is modelled",
                if default { "" } else { "no" }
            )),
            (Ok(_), _) => Ok(false),
        });
    }
    None
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
    fn repository_cache_options_parse() {
        let options = parse(
            "info",
            &[
                "--repository_cache=/a",
                "--experimental_repository_cache",
                "/b",
                "--repo_contents_cache=",
                "--experimental_repository_cache_hardlinks",
                "--experimental_repository_downloader_retries=3",
                "--experimental_remote_downloader=",
                "--noexperimental_remote_downloader_local_fallback",
                "--experimental_remote_downloader_propagate_credentials",
            ],
        )
        .unwrap();
        assert_eq!(options.repository_cache.as_deref(), Some("/b"));
        assert_eq!(options.repo_contents_cache.as_deref(), Some(""));
        assert!(options.repository_cache_hardlinks);
        assert_eq!(options.repository_downloader_retries, Some(3));
        assert_eq!(
            parse("build", &["--experimental_repository_downloader_retries=x"]).unwrap_err(),
            "While parsing option --experimental_repository_downloader_retries=x: 'x' is not an int"
        );
        assert!(
            parse("build", &["--experimental_remote_downloader=grpc://x"])
                .unwrap_err()
                .contains("the Remote Asset API downloader is not implemented")
        );
        assert!(parse("build", &["--norepository_cache"]).is_err());
    }

    #[test]
    fn code_rc_options_parse() {
        let options = parse(
            "build",
            &[
                "--attempt_to_print_relative_paths=true",
                "--repo_env=JAVA_HOME=../bazel_tools/jdk",
                "--experimental_isolated_extension_usages",
                "--incompatible_enable_proto_toolchain_resolution",
                "--repo_env",
                "NO_PROXY=x",
            ],
        )
        .unwrap();
        assert!(options.attempt_to_print_relative_paths);
        assert!(options.isolated_extension_usages);
        // N41's RemoteOptions own these (crate::remote_cache).
        for arg in [
            "--noremote_upload_local_results",
            "--remote_instance_name=dev",
        ] {
            assert_eq!(
                RcOptions::default().parse("build", arg, None, "").unwrap(),
                None,
                "{arg}"
            );
        }
        // --test_output/--test_summary belong to N23g's TestOptions for the
        // building commands (crate::test_command, tested there).
        for command in ["build", "run", "test"] {
            assert_eq!(
                RcOptions::default()
                    .parse(command, "--test_output=errors", None, "")
                    .unwrap(),
                None
            );
            assert_eq!(
                RcOptions::default()
                    .parse(command, "--test_summary", Some("terse"), "")
                    .unwrap(),
                None
            );
        }
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
        // The building commands' native parser owns --sandbox_add_mount_pair
        // (crate::sandbox::mount_pair); other commands do not have it.
        assert_eq!(
            RcOptions::default()
                .parse("build", "--sandbox_add_mount_pair=/tmp", None, "")
                .unwrap(),
            None
        );
        assert_eq!(
            error("query", &["--sandbox_add_mount_pair=/tmp"]),
            "--sandbox_add_mount_pair=/tmp :: Unrecognized option: --sandbox_add_mount_pair=/tmp"
        );
        assert_eq!(
            error("query", &["--test_output=errors"]),
            "--test_output=errors :: Unrecognized option: --test_output=errors"
        );
        assert_eq!(
            error("build", &["--attempt_to_print_relative_paths=maybe"]),
            "While parsing option --attempt_to_print_relative_paths=maybe: 'maybe' is not a boolean"
        );
        // N42 models both values of --incompatible_strict_action_env in the
        // configuration parser; RcOptions does not claim it.
        for arg in [
            "--incompatible_strict_action_env",
            "--noincompatible_strict_action_env",
            "--experimental_strict_action_env=false",
        ] {
            assert_eq!(
                RcOptions::default().parse("build", arg, None, "").unwrap(),
                None,
                "{arg}"
            );
        }
        let options = parse(
            "build",
            &["--noincompatible_enable_proto_toolchain_resolution"],
        )
        .unwrap();
        assert_eq!(options.proto_toolchain_resolution, Some(false));
    }
}
