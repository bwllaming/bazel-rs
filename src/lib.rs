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
pub mod output_group_info;
pub mod package;
pub mod package_specification;
pub mod patch_inputs;
pub mod patching;
pub mod plan_timings;
mod platform_providers;
pub mod platforms;
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
mod rule_declarations;
mod rule_errors;
pub mod run_command;
mod run_environment_info;
pub mod runfiles;
pub mod runfiles_tree;
pub mod sandbox;
pub mod scheduler;
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
/// (`BAZEL_RS_TIMING=1`): elapsed wall time and VmRSS at each mark, on
/// stderr, prefixed `PERF`. Loading and the two planning phases are marked
/// so their share of analysis wall time is measurable end to end
/// (docs/artifacts PARALLEL_ANALYSIS.md).
pub fn perf_mark(name: &str) {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    if std::env::var_os("BAZEL_RS_TIMING").is_some() {
        let rss = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .map(|l| l.split_whitespace().nth(1).unwrap_or("").to_owned())
            })
            .unwrap_or_default();
        eprintln!(
            "PERF {:>9.3}s rss={}kB {name}",
            start.elapsed().as_secs_f64(),
            rss
        );
    }
}
