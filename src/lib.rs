//! Experimental native Bazel-compatible primitives. Not a Bazel replacement yet.
// Test modules may start processes directly (clippy.toml).
#![cfg_attr(test, allow(clippy::disallowed_methods))]
#[cfg(test)]
include!("../tests/support/temp_root.rs");
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
mod aspect_propagation;
pub mod aspects;
mod attribute_values;
pub mod bazel_bytes;
mod bazel_label;
mod bazel_options;
pub mod bazelrc;
pub mod bep;
mod build_setting_ctx;
pub mod build_settings;
pub mod builtin_tools;
mod bzl_visibility;
pub mod bzlmod;
pub mod cache_gc;
pub mod canonicalize;
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
pub mod cquery_proto;
pub mod cquery_state;
pub mod credentials;
pub mod ctx_surface;
pub mod custom_rule;
pub mod debug_print;
mod depset_items;
pub mod digest_memo;
pub mod dir_stamps;
pub mod download_proxy;
pub mod engine;
mod exec_groups;
pub mod exec_log;
mod execution_info;
pub mod extension_calls;
pub mod extension_ctx;
mod extension_download;
mod extension_io;
pub mod extension_metadata;
pub(crate) mod fast_hash;
pub mod feature_flags;
pub mod flag_alias;
pub mod git_process;
pub mod git_source;
pub mod glob;
pub mod grpc_channel;
mod guarded_globals;
pub mod help;
mod help_topics;
mod host_os;
pub mod ignored;
mod implicit_outputs;
pub mod info;
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
pub mod mod_command;
pub mod module_cli;
pub mod module_extension;
pub mod module_graph;
pub mod native_rules;
pub mod observed_fs;
mod option_metadata;
pub mod output_base;
pub mod output_group_info;
pub mod output_tree;
pub mod package;
pub mod package_specification;
pub mod patch_inputs;
pub mod patching;
pub mod plan_timings;
mod platform_providers;
pub mod platforms;
mod prefetch;
pub mod process_wrapper;
pub mod profile;
pub mod progress;
pub mod providers;
mod py_regex;
pub mod query_language;
pub mod rc_options;
pub mod reapi;
pub mod registry;
pub mod registry_session;
pub mod remote_cache;
pub mod remote_execution;
pub mod remote_grpc;
pub mod remote_outputs;
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
pub mod repository_slots;
mod repository_speculation;
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
pub mod shared_path;
mod signals;
pub mod snapshot;
mod source_directories;
pub mod source_links;
pub mod spawn_lock;
pub mod starlark_options;
mod starlark_transitions;
pub mod starlark_types;
mod subrule;
mod subrule_fragments;
mod symbolic_macro;
mod target_pattern;
pub mod target_pattern_file;
mod template_dict;
pub mod template_variable_info;
pub mod test_command;
pub mod test_runner;
mod toolchain_info;
pub mod toolchains;
pub mod ui;
pub mod visibility;
pub mod worker;
pub mod workspace;
pub mod workspace_status;
pub mod worktree;

/// Phase timestamps for the parallel-analysis baseline and gate
/// (`BAZEL_RS_TIMING=1`): elapsed wall time, VmRSS and the peak RSS so far
/// (VmHWM) at each mark, on stderr, prefixed `PERF`. Loading and the two
/// planning phases are marked so their share of analysis wall time is
/// measurable end to end (docs/PARALLEL_ANALYSIS.md, docs/PERF_LAYER1.md).
///
/// Line shape: `PERF <t>s rss=<kB>kB <mark> hwm=<kB>kB`. The peak is
/// appended after the mark name so parsers of the original
/// `PERF <t>s rss=<kB>kB <mark>` prefix keep working unchanged.
/// The process's start (`main.start`): the monotonic clock and the wall
/// clock of one instant, the origin of the build event and profile times.
pub fn process_clock() -> (std::time::Instant, std::time::SystemTime) {
    static START: std::sync::OnceLock<(std::time::Instant, std::time::SystemTime)> =
        std::sync::OnceLock::new();
    *START.get_or_init(|| (std::time::Instant::now(), std::time::SystemTime::now()))
}

pub fn perf_mark(name: &str) {
    // The end of planning (build, test and run all mark it right after
    // `plan_build`) closes the analysis-snapshot recording window
    // (crate::observed_fs, BAZEL_RS_SNAPSHOT=record); a no-op otherwise.
    if name == "plan.end" {
        observed_fs::close_window();
    }
    // The interactive progress display's phases (a no-op when it is off).
    progress::mark(name);
    let (start, _) = process_clock();
    // The profile's phase markers (a no-op without --profile).
    profile::mark(name);
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

/// Timeline spans of the external-repository phase (`BAZEL_RS_TIMING_TIMELINE=1`,
/// docs/MODULE_EXTENSIONS.md "Profiling"): one `PERF-TL <start>s <end>s
/// <seconds> <thread> <kind> <name>` line per module extension evaluation
/// (`extension`), repository materialization (`executed`, `reused`),
/// `execute()` subprocess and download, relative to the first PERF mark —
/// which runs where, and what waits for what.
pub mod timeline {
    use std::sync::OnceLock;
    use std::time::Instant;

    /// Whether spans are recorded.
    pub fn enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED
            .get_or_init(|| crate::diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING_TIMELINE")))
    }

    /// A span from `start` until now.
    pub fn record(kind: &str, name: &str, start: Instant) {
        if !enabled() {
            return;
        }
        let (origin, _) = super::process_clock();
        let begin = start.saturating_duration_since(origin).as_secs_f64();
        let end = origin.elapsed().as_secs_f64();
        let thread = std::thread::current();
        crate::snapshot::diag::line_fmt(format_args!(
            "PERF-TL {begin:9.3}s {end:9.3}s {:8.3} {} {kind} {name}",
            end - begin,
            thread.name().unwrap_or("-")
        ));
    }

    /// A span that ends when it is dropped (`name` is formed only when
    /// spans are recorded).
    pub struct Span(Option<(Instant, &'static str, String)>);

    pub fn span(kind: &'static str, name: impl FnOnce() -> String) -> Span {
        Span(enabled().then(|| (Instant::now(), kind, name())))
    }

    impl Drop for Span {
        fn drop(&mut self) {
            if let Some((start, kind, name)) = self.0.take() {
                record(kind, &name, start);
            }
        }
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
