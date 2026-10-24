//! Experimental native Bazel-compatible primitives. Not a Bazel replacement yet.
// Test modules may start processes directly (clippy.toml).
#![cfg_attr(test, allow(clippy::disallowed_methods))]
/// Semantics targeted by the native implementation, not a full-parity claim.
pub const COMPATIBILITY_VERSION: &str = "9.3.0";

pub mod action;
pub mod action_env;
pub mod analysis;
mod analysis_ctx;
mod analysis_ctx_surface;
mod analysis_runtime;
mod apple_xcode;
pub mod archive;
pub mod args;
pub mod aspects;
mod attribute_values;
pub mod bazel_bytes;
mod bazel_label;
mod bazel_options;
pub mod bazelrc;
mod build_setting_ctx;
pub mod build_settings;
pub mod builtin_tools;
mod bzl_visibility;
pub mod bzlmod;
mod cc_actions;
mod cc_common;
mod cc_features;
mod cc_variables;
mod command_line;
mod command_options;
pub mod config_registry;
pub mod config_setting;
pub mod configuration;
pub mod configured_attributes;
pub mod credentials;
pub mod ctx_surface;
pub mod custom_rule;
mod depset_items;
pub mod digest_memo;
pub mod download_proxy;
pub mod engine;
mod exec_groups;
mod execution_info;
pub mod extension_calls;
pub mod extension_ctx;
mod extension_download;
mod extension_io;
pub mod extension_metadata;
pub mod feature_flags;
pub mod git_process;
pub mod git_source;
pub mod glob;
mod guarded_globals;
mod host_os;
pub mod ignored;
mod implicit_outputs;
mod instrumented_files;
mod json;
pub mod label;
pub mod language_modules;
mod late_bound;
pub mod loading;
mod location_expander;
pub mod location_printer;
pub mod lockfile;
mod lockfile_extensions;
pub mod materializer;
pub mod memory_limit;
pub mod module_cli;
pub mod module_extension;
pub mod module_graph;
pub mod native_rules;
pub mod observed_fs;
pub mod output_group_info;
pub mod package;
pub mod package_specification;
pub mod patch_inputs;
pub mod patching;
pub mod plan_timings;
mod platform_providers;
pub mod platforms;
mod prefetch;
pub mod process_wrapper;
pub mod providers;
pub mod query_language;
pub mod rc_options;
pub mod reapi;
pub mod registry;
pub mod registry_session;
pub mod remote_cache;
pub mod repo_env;
pub mod repository;
mod repository_budget;
pub mod repository_catalog;
pub mod repository_ctx;
pub mod repository_demand;
mod repository_environment;
pub mod repository_links;
mod repository_process;
pub mod repository_rule;
pub mod repository_store;
pub mod repository_text;
mod repository_warm;
pub mod request;
mod rule_declarations;
mod rule_errors;
pub mod run_command;
mod run_environment_info;
pub mod runfiles;
pub mod runfiles_tree;
pub mod sandbox;
pub mod scheduler;
mod shared_actions;
pub mod snapshot;
pub mod source_links;
pub mod spawn_lock;
pub mod starlark_options;
mod starlark_transitions;
pub mod starlark_types;
mod subrule;
mod subrule_fragments;
mod symbolic_macro;
mod target_pattern;
mod template_dict;
pub mod template_variable_info;
pub mod test_command;
pub mod test_runner;
mod toolchain_info;
pub mod toolchains;
pub mod visibility;
pub mod workspace;
pub mod workspace_status;

/// Phase timestamps for the parallel-analysis baseline and gate
/// (`BAZEL_RS_TIMING=1`): elapsed wall time, VmRSS and the peak RSS so far
/// (VmHWM) at each mark, on stderr, prefixed `PERF`. Loading and the two
/// planning phases are marked so their share of analysis wall time is
/// measurable end to end (docs/PARALLEL_ANALYSIS.md, docs/PERF_LAYER1.md).
///
/// Line shape: `PERF <t>s rss=<kB>kB <mark> hwm=<kB>kB`. The peak is
/// appended after the mark name so parsers of the original
/// `PERF <t>s rss=<kB>kB <mark>` prefix keep working unchanged.
pub fn perf_mark(name: &str) {
    use std::sync::OnceLock;
    // The end of planning (build, test and run all mark it right after
    // `plan_build`) closes the analysis-snapshot recording window
    // (crate::observed_fs, BAZEL_RS_SNAPSHOT=record); a no-op otherwise.
    if name == "plan.end" {
        observed_fs::close_window();
    }
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    if diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING")) {
        let status = proc_self_status();
        let (rss, hwm) = (
            proc_status_value(&status, "VmRSS:"),
            proc_status_value(&status, "VmHWM:"),
        );
        // The diagnostics stream: never part of what an analysis snapshot
        // captures of planning's stderr (crate::snapshot::diag).
        snapshot::diag::line_fmt(format_args!(
            "PERF {:>9.3}s rss={}kB {name} hwm={}kB",
            start.elapsed().as_secs_f64(),
            rss,
            hwm
        ));
    }
}

/// Whether a diagnostics switch the caller read (`BAZEL_RS_TIMING`,
/// `BAZEL_RS_TIMING_ALLOC`) is on: set to anything but empty or `0`, so
/// `=0` turns a switch off (integration-20: `BAZEL_RS_TIMING` used to count
/// as on whenever it was set; its readers — the PERF marks, the stage
/// report, PERF-REPO and PERF-DMEMO — and the allocation breakdown now
/// share this rule).
pub fn diagnostic_switch(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// This process's `/proc/self/status` (empty when unreadable): telemetry of
/// the diagnostics switches (PERF marks, the `BAZEL_RS_TIMING_ALLOC`
/// breakdown), never an input of planning — the one reader, here in an
/// allowlisted module of the analysis-snapshot discipline
/// (tests/observed_fs_discipline.rs; the strace audit ignores `/proc`).
pub(crate) fn proc_self_status() -> String {
    std::fs::read_to_string("/proc/self/status").unwrap_or_default()
}

/// The value column of one `/proc/self/status` field (`VmRSS:`, `VmHWM:`
/// in kB as the kernel prints them, `Threads:`); empty when absent
/// (non-Linux).
pub(crate) fn proc_status_value<'a>(status: &'a str, field: &str) -> &'a str {
    status
        .lines()
        .find(|line| line.starts_with(field))
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
}

#[cfg(test)]
mod perf_mark_tests {
    #[test]
    fn proc_status_fields_are_read_by_name() {
        let status = "Name:\tbazel-rs\nVmHWM:\t  204800 kB\nVmRSS:\t  102400 kB\n";
        assert_eq!(super::proc_status_value(status, "VmRSS:"), "102400");
        assert_eq!(super::proc_status_value(status, "VmHWM:"), "204800");
        assert_eq!(super::proc_status_value(status, "VmSwap:"), "");
    }

    #[test]
    fn diagnostic_switches_are_off_when_unset_empty_or_zero() {
        use super::diagnostic_switch;
        assert!(!diagnostic_switch(None));
        assert!(!diagnostic_switch(Some("".into())));
        assert!(!diagnostic_switch(Some("0".into())));
        assert!(diagnostic_switch(Some("1".into())));
        assert!(diagnostic_switch(Some("yes".into())));
    }
}
