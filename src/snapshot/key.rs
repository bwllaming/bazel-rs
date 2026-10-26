//! The snapshot key G (PLAN-L2 §1a): blake3 of the postcard encoding of
//! everything the command fixes before the snapshot check point — what a
//! snapshot cannot re-validate because it was settled before the recording
//! window opened. Computed in well under a millisecond (plus one stat of the
//! binary, crate::snapshot::exe_id).
//!
//! | part | note |
//! |---|---|
//! | snapshot format, `CARGO_PKG_VERSION`, binary identity | a new binary never reads an old snapshot |
//! | command | `build`, `test` or `run` |
//! | canonical workspace root and cache dir, the working directory, its package and its directory below the root | absolute paths are baked into the plan |
//! | startup options | they decide the rc files read (their options are below) |
//! | option units | `options.priority` after rc and `--config` expansion, as (option, value) text, minus [`EXCLUDED`] |
//! | targets | the target patterns; for `run` only the target (its arguments are used after execution) |
//! | `BZLMOD_ALLOW_YANKED_VERSIONS` | read before the window |
//! | validation policy | per tree, or full under `--verify_repo_tampering` / `BAZEL_RS_VERIFY_REPOS=full` (the snapshot holds what its policy validates) |
//!
//! [`EXCLUDED`] holds only options whose parsed value was traced to
//! execution, reporting or the code before the check point; everything
//! else — anything unsure — stays in G: including too much only costs
//! misses.

use serde::Serialize;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The snapshot format (the file layout of crate::snapshot::format and
/// the payloads it frames). 2 (L2-W5f): ENV records lost the `fixed` flag
/// (validated against the repository environment), INPUTS gained the
/// workspace-status flag, per-tree INTEGRITY keeps directories. 3 (P3):
/// the PLAN section is stored compressed (crate::snapshot::format::
/// compress_plan).
pub const FORMAT: u32 = 3;

/// Options left out of G (PLAN-L2 §1a "Options left out of G"), by name
/// (an `--[no]name` boolean's `no` form matches too). Each was traced to
/// where its parsed value flows (src/main.rs):
///
/// * `jobs`: `ExecuteOptions::jobs` and the test runner only.
/// * `loading_phase_threads`, `memory-limit`: the loading worker count and
///   the watchdog; planning output is byte-identical at any thread count
///   (the determinism gate, docs/PARALLEL_ANALYSIS.md, tests/planned_request.rs).
/// * `show_result`, `announce_rc`: the result printing after execution, the
///   rc notes before the check point.
/// * test execution (crate::test_command::TestOptions read only by the test
///   runner and `run` of a test): `test_output`, `test_summary`,
///   `cache_test_results`, `test_arg`, `test_filter`, `test_timeout`,
///   `flaky_test_attempts`, `runs_per_test`, `test_sharding_strategy`,
///   `max_test_output_bytes`, `local_test_jobs`, `test_tmpdir`,
///   `test_strategy`, `verbose_test_summary`,
///   `test_verbose_timeout_warnings`, `test_runner_fail_fast`,
///   `zip_undeclared_test_outputs`, `incompatible_check_sharding_support`.
///   The test filters (`test_tag_filters`, `test_size_filters`,
///   `test_timeout_filters`, `test_lang_filters`, `build_tests_only`) and
///   `expand_test_suites` select the targets and tests before planning (the
///   target pattern phase, crate::engine::Engine::target_pattern_phase,
///   whose expanded sets the planned request carries): they stay in G.
/// * run (crate::run_command::RunOptions, read after the build part):
///   `script_path`, `run_env`, `run`, `run_in_cwd`, `omit_run_args`.
///   `run_under` stays: a label prefix is rejected before planning.
/// * spawn strategies and the sandbox (crate::sandbox::SpawnOptions, read
///   by action execution and the test runner only), and the persistent
///   worker options (crate::worker::WorkerOptions, read by action execution).
/// * the remote cache and remote execution (crate::remote_cache, read by
///   action execution: `--remote_default_exec_properties` joins a spawn's
///   recorded platform at execution, crate::remote_execution) and
///   credential helpers (read by downloads — network, which a saved run
///   never used — and the remote cache).
/// * `native-action-dump`: written from the planned request on every run.
/// * the build event files, the profile and the execution log
///   (crate::bep, crate::profile, crate::exec_log): written after or beside
///   planning from the planned request (which always carries the build
///   event plan); `bes_backend` is rejected before the check point.
/// * the UI options (crate::progress: `curses`, `color`, `show_progress`,
///   `show_progress_rate_limit`, `progress_in_terminal_title`,
///   `ui_actions_shown` and its old name): read by the progress display
///   only, whose lines never enter a snapshot window.
pub const EXCLUDED: &[&str] = &[
    "jobs",
    "loading_phase_threads",
    "memory-limit",
    "show_result",
    "announce_rc",
    "test_output",
    "test_summary",
    "cache_test_results",
    "test_arg",
    "test_filter",
    "test_timeout",
    "flaky_test_attempts",
    "runs_per_test",
    "test_sharding_strategy",
    "max_test_output_bytes",
    "local_test_jobs",
    "test_tmpdir",
    "test_strategy",
    "verbose_test_summary",
    "test_verbose_timeout_warnings",
    "test_runner_fail_fast",
    "zip_undeclared_test_outputs",
    "incompatible_check_sharding_support",
    "script_path",
    "run_env",
    "run",
    "run_in_cwd",
    "omit_run_args",
    "spawn_strategy",
    "genrule_strategy",
    "strategy",
    "sandbox_debug",
    "sandbox_default_allow_network",
    "experimental_sandbox_default_allow_network",
    "sandbox_writable_path",
    "sandbox_block_path",
    "sandbox_tmpfs_path",
    "worker_max_instances",
    "worker_max_multiplex_instances",
    "experimental_worker_max_multiplex_instances",
    "worker_quit_after_build",
    "worker_verbose",
    "worker_extra_flag",
    "worker_sandboxing",
    "worker_multiplex",
    "experimental_worker_multiplex",
    "experimental_worker_multiplex_sandboxing",
    "experimental_worker_strict_flagfiles",
    "experimental_worker_allowlist",
    "sandbox_add_mount_pair",
    "remote_cache",
    "remote_executor",
    "remote_header",
    "remote_cache_header",
    "remote_instance_name",
    "remote_timeout",
    "remote_upload_local_results",
    "remote_accept_cached",
    "remote_http_cache",
    "remote_exec_header",
    "remote_default_exec_properties",
    "remote_local_fallback",
    "remote_local_fallback_strategy",
    "remote_print_execution_messages",
    "remote_execution_priority",
    "remote_result_cache_priority",
    "credential_helper",
    "credential_helper_timeout",
    "credential_helper_cache_duration",
    "experimental_credential_helper",
    "experimental_credential_helper_timeout",
    "experimental_credential_helper_cache_duration",
    "native-action-dump",
    "curses",
    "color",
    "show_progress",
    "show_progress_rate_limit",
    "progress_in_terminal_title",
    "ui_actions_shown",
    "experimental_ui_actions_shown",
    "build_event_json_file",
    "build_event_binary_file",
    "build_event_text_file",
    "build_event_json_file_path_conversion",
    "experimental_build_event_json_file_path_conversion",
    "build_event_binary_file_path_conversion",
    "experimental_build_event_binary_file_path_conversion",
    "build_event_text_file_path_conversion",
    "experimental_build_event_text_file_path_conversion",
    "build_event_json_file_upload_mode",
    "build_event_binary_file_upload_mode",
    "build_event_text_file_upload_mode",
    "build_event_max_named_set_of_file_entries",
    "build_event_publish_all_actions",
    "build_metadata",
    "bes_backend",
    "profile",
    "generate_json_trace_profile",
    "experimental_generate_json_trace_profile",
    "experimental_profile_include_target_label",
    "experimental_profile_include_primary_output",
    "slim_profile",
    "experimental_slim_json_profile",
    "record_full_profiler_data",
    "experimental_announce_profile_path",
    "experimental_profile_additional_tasks",
    "experimental_collect_load_average_in_profiler",
    "experimental_collect_system_network_usage",
    "experimental_collect_resource_estimation",
    "experimental_collect_pressure_stall_indicators",
    "experimental_collect_skyframe_counts_in_profiler",
    "experimental_collect_worker_data_in_profiler",
    "experimental_profile_include_target_configuration",
    "experimental_run_bep_event_include_residue",
    "experimental_command_profile",
    "memory_profile",
    "memory_profile_stable_heap_parameters",
    "experimental_profile_cpu_usage",
    "execution_log_json_file",
    "execution_log_binary_file",
    "execution_log_compact_file",
    "experimental_execution_log_compact_file",
    "execution_log_sort",
];

/// The option name of an option argument: `--name=value` / `--name` /
/// `-name` without dashes and value.
fn option_name(text: &str) -> &str {
    let name = text.trim_start_matches('-');
    name.split_once('=').map_or(name, |(name, _)| name)
}

/// Whether the option argument `text` is left out of G ([`EXCLUDED`]).
pub fn excluded(text: &str) -> bool {
    let name = option_name(text);
    EXCLUDED.contains(&name)
        || name
            .strip_prefix("no")
            .is_some_and(|name| EXCLUDED.contains(&name))
}

/// What G is computed from.
pub struct Inputs<'a> {
    pub command: &'a str,
    /// Canonical when it exists (the cache may not exist yet).
    pub workspace: &'a Path,
    pub cache: &'a Path,
    pub cwd: &'a Path,
    pub relative_directory: &'a str,
    pub current_package: &'a str,
    pub startup: &'a [String],
    pub options: &'a [crate::bazelrc::Unit],
    pub targets: &'a [String],
    pub allow_yanked: Option<&'a str>,
    pub full_validation: bool,
}

#[derive(Serialize)]
struct Parts<'a> {
    format: u32,
    version: &'a str,
    binary: [u8; 32],
    command: &'a str,
    workspace: &'a [u8],
    cache: &'a [u8],
    cwd: &'a [u8],
    relative_directory: &'a str,
    current_package: &'a str,
    startup: Vec<&'a str>,
    options: Vec<(&'a str, Option<&'a str>)>,
    targets: &'a [String],
    allow_yanked: Option<&'a str>,
    full_validation: bool,
}

/// A snapshot's key: G and the binary identity it includes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub g: [u8; 32],
    pub binary: [u8; 32],
}

impl Key {
    /// The first 16 bytes of G in hex: the snapshot's file name
    /// (crate::snapshot::store) and its name in diagnostics.
    pub fn short(&self) -> String {
        crate::observed_fs::hex(&self.g[..16])
    }
}

/// G of `inputs` for the binary `binary`.
pub fn compute(inputs: &Inputs<'_>, binary: [u8; 32]) -> Key {
    let parts = Parts {
        format: FORMAT,
        version: env!("CARGO_PKG_VERSION"),
        binary,
        command: inputs.command,
        workspace: inputs.workspace.as_os_str().as_bytes(),
        cache: inputs.cache.as_os_str().as_bytes(),
        cwd: inputs.cwd.as_os_str().as_bytes(),
        relative_directory: inputs.relative_directory,
        current_package: inputs.current_package,
        // `--[no]quiet` is console-only (crate::ui), like Bazel's: it never
        // restarts the server, and a snapshot replays filtered.
        startup: inputs
            .startup
            .iter()
            .map(String::as_str)
            .filter(|arg| !matches!(*arg, "--quiet" | "--noquiet"))
            .collect(),
        options: inputs
            .options
            .iter()
            .filter(|unit| !excluded(&unit.option.text))
            .map(|unit| {
                (
                    unit.option.text.as_str(),
                    unit.value.as_ref().map(|value| value.text.as_str()),
                )
            })
            .collect(),
        targets: inputs.targets,
        allow_yanked: inputs.allow_yanked,
        full_validation: inputs.full_validation,
    };
    let bytes = postcard::to_stdvec(&parts).expect("the key parts encode");
    Key {
        g: *blake3::hash(&bytes).as_bytes(),
        binary,
    }
}

/// The test or debug hook that keeps this process from reading or writing
/// snapshots (PLAN-L2 §1a: crate::snapshot::record::TEST_HOOKS and every
/// `BAZEL_RS_DEBUG_*`), if one is set.
pub fn test_hook() -> Option<String> {
    std::env::vars_os().find_map(|(name, _)| {
        let name = name.to_string_lossy();
        (crate::snapshot::record::TEST_HOOKS.contains(&name.as_ref())
            || name.starts_with("BAZEL_RS_DEBUG_"))
        .then(|| name.into_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bazelrc::{Arg, Origin, Unit};

    fn unit(option: &str, value: Option<&str>) -> Unit {
        let arg = |text: &str| Arg {
            text: text.to_owned(),
            origin: Origin::CommandLine,
        };
        Unit {
            option: arg(option),
            value: value.map(arg),
        }
    }

    fn key_of(options: &[Unit], targets: &[&str]) -> Key {
        key_with_startup(&[], options, targets)
    }

    fn key_with_startup(startup: &[String], options: &[Unit], targets: &[&str]) -> Key {
        let targets: Vec<String> = targets.iter().map(|t| (*t).to_owned()).collect();
        compute(
            &Inputs {
                command: "build",
                workspace: Path::new("/ws"),
                cache: Path::new("/ws/bazel-rs-out"),
                cwd: Path::new("/ws"),
                relative_directory: "",
                current_package: "",
                startup,
                options,
                targets: &targets,
                allow_yanked: None,
                full_validation: false,
            },
            [1; 32],
        )
    }

    #[test]
    fn quiet_is_left_out_and_other_startup_options_count() {
        let startup =
            |args: &[&str]| -> Vec<String> { args.iter().map(|a| a.to_string()).collect() };
        let base = key_of(&[], &["//..."]);
        for args in [&["--quiet"][..], &["--noquiet"], &["--quiet", "--noquiet"]] {
            assert_eq!(key_with_startup(&startup(args), &[], &["//..."]), base);
        }
        assert_ne!(
            key_with_startup(&startup(&["--nohome_rc"]), &[], &["//..."]),
            base
        );
    }

    #[test]
    fn execution_only_options_are_left_out_and_everything_else_counts() {
        for text in [
            "--jobs=8",
            "--jobs",
            "--loading_phase_threads=4",
            "--memory-limit=4096",
            "--show_result=10",
            "--test_output=errors",
            "--nocache_test_results",
            "--test_filter=x",
            "--norun",
            "--script_path=s",
            "--spawn_strategy=standalone",
            "--worker_max_instances=2",
            "--noworker_multiplex",
            "--remote_cache=",
            "--credential_helper=x",
            "--native-action-dump=/tmp/d",
        ] {
            assert!(excluded(text), "{text}");
        }
        for text in [
            "--nobuild",
            "--test_tag_filters=a",
            "--test_size_filters=small",
            "--test_timeout_filters=short",
            "--test_lang_filters=sh",
            "--build_tests_only",
            "--nobuild_tests_only",
            "--expand_test_suites",
            "--noexpand_test_suites",
            "--run_under=x",
            "--action_env=A",
            "--repo_env=A=b",
            "--lockfile_mode=update",
            "--registry=file:///r",
            "--experimental-builtin-modules",
            "--//pkg:flag=1",
            "--define=a=b",
            "--norunfiles_check",
            "--keep_going",
        ] {
            assert!(!excluded(text), "{text}");
        }
        let base = key_of(&[unit("--nobuild", None)], &["//..."]);
        assert_eq!(
            key_of(
                &[
                    unit("--nobuild", None),
                    unit("--jobs", Some("3")),
                    unit("--show_result=4", None)
                ],
                &["//..."]
            ),
            base
        );
        assert_ne!(key_of(&[], &["//..."]), base);
        assert_ne!(key_of(&[unit("--nobuild", None)], &["//:a"]), base);
        assert_ne!(
            key_of(
                &[unit("--nobuild", None), unit("--define", Some("a=b"))],
                &["//..."]
            ),
            base
        );
        assert_eq!(base.short().len(), 32);
    }
}
