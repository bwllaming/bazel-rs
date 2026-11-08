//! `BAZEL_RS_EVAL_VERIFY=1` (docs/EVALUATOR.md "Test strategy"): the
//! dual-engine oracle. The build plans twice — the walk on a fork of the
//! engine ([`Engine::fork_for_verify`]: the same pre-planning state, one
//! `Arc<RepositorySet>`, so repository work is done once), its console
//! captured; then the evaluator on this engine, its console captured — and
//! compares, in order ([`Compared`]): the phase-1 plan (keys and order),
//! the configured nodes, the toolchain/exec-group contexts, the
//! delegations (and the other linearizer-only phase-1 maps), the action
//! dump (BAZEL_RS_DUMP_ACTIONS form) and the planning result, stderr (the
//! console writes), the incremental index (BAZEL_RS_SNAPSHOT_INDEX_DUMP
//! form) and the cquery graph. The first difference is an internal error
//! naming the stage, the key and the field; otherwise the walk's console
//! is written (the evaluator's but for the repository set's shared work,
//! which only the walk did) and the evaluator's plan used.
//!
//! Process-wide state the walk would leave behind is restored before the
//! evaluator plans (the progress package/aspect counters, the printed
//! `--toolchain_resolution_debug` events). Console writes of work shared
//! through the repository set (repository rules, module extensions, first
//! `.bzl` evaluations: `crate::ui::SharedWork`) are done once, by the walk:
//! when only they differ, the consoles agree. Known limit: process-wide
//! test hooks that fire once (e.g. `BAZEL_RS_LOWERING_FAIL`) fire in the
//! walk only.

use std::collections::BTreeMap;
use std::fmt::Debug;

use super::super::{
    BuildPlan, ConfiguredNodes, ConfiguredTargetKey, Engine, KeyMap, Plan, root_checkpoint,
};
use super::AnalysisMode;

/// Whether `BAZEL_RS_EVAL_VERIFY=1` asks for the comparison.
pub(crate) fn enabled() -> bool {
    // snapshot-unrecorded: a diagnostic switch; it never changes the plan.
    match std::env::var_os("BAZEL_RS_EVAL_VERIFY") {
        Some(value) => value == "1",
        // The test-suite mode (a binary built with BAZEL_RS_DEFAULT_EVAL_VERIFY=1).
        None => option_env!("BAZEL_RS_DEFAULT_EVAL_VERIFY") == Some("1"),
    }
}

/// What the comparison compares, in order (each a stage of the oracle;
/// the first difference stops it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Compared {
    Plan,
    ConfiguredNodes,
    Contexts,
    Delegations,
    Actions,
    Stderr,
    IncrementalIndex,
    CqueryGraph,
}

/// What one engine's phase 1 leaves only in locals (the comparison reads
/// it at the end of phase 1).
#[derive(Default)]
pub(crate) struct Observed {
    plan: Vec<ConfiguredTargetKey>,
    /// Each configured node's Debug digest.
    configured: BTreeMap<ConfiguredTargetKey, u64>,
    done: bool,
}

/// A digest of `value`'s Debug text.
fn debug_digest(value: &impl Debug) -> u64 {
    use std::hash::Hasher;
    struct Writer(crate::fast_hash::FxHasher);
    impl std::fmt::Write for Writer {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0.write(text.as_bytes());
            Ok(())
        }
    }
    let mut writer = Writer(crate::fast_hash::FxHasher::default());
    let _ = std::fmt::write(&mut writer, format_args!("{value:?}"));
    writer.0.finish()
}

fn digests<V: Debug>(map: &KeyMap<V>) -> BTreeMap<ConfiguredTargetKey, u64> {
    map.iter()
        .map(|(key, value)| (key.clone(), debug_digest(value)))
        .collect()
}

/// The first difference of two keyed digests.
fn map_difference<K: Ord + Debug, V: PartialEq + Debug>(
    what: &str,
    walk: &BTreeMap<K, V>,
    eval: &BTreeMap<K, V>,
) -> Option<String> {
    for (key, value) in walk {
        match eval.get(key) {
            None => return Some(format!("{what}: {key:?} only in the walk")),
            Some(other) if other != value => {
                return Some(format!("{what}: {key:?} differs ({value:?} vs {other:?})"));
            }
            Some(_) => {}
        }
    }
    eval.keys()
        .find(|key| !walk.contains_key(key))
        .map(|key| format!("{what}: {key:?} only in the evaluator"))
}

/// The first differing line of two texts.
fn text_difference(what: &str, walk: &str, eval: &str) -> Option<String> {
    if walk == eval {
        return None;
    }
    let (walk_lines, eval_lines): (Vec<&str>, Vec<&str>) =
        (walk.lines().collect(), eval.lines().collect());
    let index = walk_lines
        .iter()
        .zip(&eval_lines)
        .position(|(walk, eval)| walk != eval)
        .unwrap_or(walk_lines.len().min(eval_lines.len()));
    // The section of a Debug dump the line is in (`    name: [`).
    let section = walk_lines[..index.min(walk_lines.len())]
        .iter()
        .rev()
        .find(|line| {
            line.starts_with("    ") && !line.starts_with("     ") && line.ends_with(": [")
        })
        .map(|line| line.trim())
        .unwrap_or("");
    Some(format!(
        "{what} differ at line {} {section}({}/{} lines):\n    walk: {:?}\n    eval: {:?}",
        index + 1,
        walk_lines.len(),
        eval_lines.len(),
        walk_lines.get(index),
        eval_lines.get(index)
    ))
}

/// The console writes as text; `own`: without the repository set's
/// shared work (done once, by the first engine: [`crate::ui::SharedWork`]).
fn console_text(writes: &[crate::ui::Deferred], own: bool) -> String {
    writes
        .iter()
        .filter(|write| !(own && matches!(write, crate::ui::Deferred::Shared(_))))
        .map(|write| format!("{write:?}\n"))
        .collect()
}

impl Engine {
    /// The end of phase 1 under verification: what the comparison reads of
    /// its locals.
    pub(in crate::engine) fn eval_observe_phase1(
        &mut self,
        plan: &Plan,
        configured: &ConfiguredNodes,
    ) {
        if let Some(observed) = self.eval_verifying.as_mut()
            && !observed.done
        {
            observed.done = true;
            observed.plan = plan.iter().map(|(key, _)| key.clone()).collect();
            observed.configured = configured
                .iter()
                .map(|(key, node)| (key.clone(), debug_digest(node)))
                .collect();
        }
    }

    /// `plan_build` under BAZEL_RS_EVAL_VERIFY=1: the walk on a fork, then
    /// the evaluator here, compared.
    pub(in crate::engine) fn plan_build_verified(
        &mut self,
        labels: &[crate::label::Label],
        checkpoint: Option<&mut root_checkpoint::Callback<'_>>,
    ) -> Result<root_checkpoint::Outcome<BuildPlan>, String> {
        // Process-wide state the shadow analysis must not leave behind.
        let counts = crate::progress::analysis_counts();
        let debug_events = crate::toolchains::printed_debug_events();
        let mut walk = self.fork_for_verify();
        walk.eval_mode = Some(AnalysisMode::Walk);
        walk.eval_verifying = Some(Box::default());
        let console = crate::ui::DeferredConsole::start();
        let walk_result = walk.plan_build_with_checkpoint(labels, None);
        let walk_console = console.finish();
        crate::progress::restore_analysis_counts(counts);
        crate::toolchains::restore_printed_debug_events(debug_events);

        let mode = self.eval_mode.replace(AnalysisMode::Eval);
        self.eval_verifying = Some(Box::default());
        let console = crate::ui::DeferredConsole::start();
        let result = self.plan_build_with_checkpoint(labels, checkpoint);
        let eval_console = console.finish();
        self.eval_mode = mode;

        // snapshot-unrecorded: a diagnostics dump; it changes nothing the analysis computes.
        if let Some(directory) = std::env::var_os("BAZEL_RS_EVAL_VERIFY_DUMP") {
            let directory = std::path::PathBuf::from(directory);
            // snapshot-unrecorded: writes the diagnostics dump requested above.
            let _ = std::fs::write(
                directory.join("walk.console"),
                console_text(&walk_console, false),
            );
            // snapshot-unrecorded: writes the diagnostics dump requested above.
            let _ = std::fs::write(
                directory.join("eval.console"),
                console_text(&eval_console, false),
            );
        }
        let difference = self.verify_difference(
            &mut walk,
            labels,
            &walk_result,
            &result,
            &walk_console,
            &eval_console,
        );
        self.eval_verifying = None;
        // The console is the walk's: equal to the evaluator's but for the
        // repository set's shared work, which only the walk did (and whose
        // output is part of the sequential console).
        crate::ui::replay(if difference.is_none() {
            walk_console
        } else {
            eval_console
        });
        // The fork is dropped like the engine itself (never torn down).
        std::mem::forget(walk);
        match difference {
            Some((stage, difference)) => Err(format!(
                "internal error: BAZEL_RS_EVAL_VERIFY: the evaluator differs from the walk ({stage:?}): {difference}"
            )),
            None => result,
        }
    }

    /// The phase-1 state of a successful analysis compared.
    fn state_difference(&mut self, walk: &mut Engine) -> Option<(Compared, String)> {
        let (walk_observed, eval_observed) = (
            walk.eval_verifying.take().unwrap_or_default(),
            self.eval_verifying.take().unwrap_or_default(),
        );
        if walk_observed.plan != eval_observed.plan {
            let index = walk_observed
                .plan
                .iter()
                .zip(&eval_observed.plan)
                .position(|(walk, eval)| walk != eval)
                .unwrap_or(walk_observed.plan.len().min(eval_observed.plan.len()));
            return Some((
                Compared::Plan,
                format!(
                    "entry {index} ({}/{} entries): walk {:?}, eval {:?}",
                    walk_observed.plan.len(),
                    eval_observed.plan.len(),
                    walk_observed.plan.get(index).map(ToString::to_string),
                    eval_observed.plan.get(index).map(ToString::to_string)
                ),
            ));
        }
        if let Some(difference) = map_difference(
            "configured node",
            &walk_observed.configured,
            &eval_observed.configured,
        ) {
            return Some((Compared::ConfiguredNodes, difference));
        }
        for (what, walk_map, eval_map) in [
            (
                "toolchain context",
                digests(&walk.toolchain_contexts),
                digests(&self.toolchain_contexts),
            ),
            (
                "exec group contexts",
                digests(&walk.exec_group_contexts),
                digests(&self.exec_group_contexts),
            ),
            (
                "exec configuration",
                digests(&walk.exec_configurations),
                digests(&self.exec_configurations),
            ),
        ] {
            if let Some(difference) = map_difference(what, &walk_map, &eval_map) {
                return Some((Compared::Contexts, difference));
            }
        }
        if walk.rule_delegations != self.rule_delegations {
            return Some((
                Compared::Delegations,
                "the rule delegations differ".to_owned(),
            ));
        }
        if walk.empty_configuration_rules != self.empty_configuration_rules {
            return Some((
                Compared::Delegations,
                "the empty-configuration rules differ".to_owned(),
            ));
        }
        if walk.toolchain_implementations != self.toolchain_implementations {
            return Some((
                Compared::Delegations,
                "the toolchain implementations differ".to_owned(),
            ));
        }
        let configured_rules = |engine: &Engine| -> Vec<ConfiguredTargetKey> {
            engine.configured_rules.keys().cloned().collect()
        };
        if configured_rules(walk) != configured_rules(self) {
            return Some((
                Compared::Delegations,
                "the configured rules differ".to_owned(),
            ));
        }
        None
    }

    fn verify_difference(
        &mut self,
        walk: &mut Engine,
        labels: &[crate::label::Label],
        walk_result: &Result<root_checkpoint::Outcome<BuildPlan>, String>,
        result: &Result<root_checkpoint::Outcome<BuildPlan>, String>,
        walk_console: &[crate::ui::Deferred],
        eval_console: &[crate::ui::Deferred],
    ) -> Option<(Compared, String)> {
        // A cquery root checkpoint (the evaluator's only) ends planning
        // early: nothing comparable.
        if let Ok(root_checkpoint::Outcome::AcceptedQuery(_)) = result {
            return None;
        }
        // One failed where the other did not: the failure first.
        if walk_result.is_err() != result.is_err() {
            let error = |result: &Result<_, String>| result.as_ref().err().cloned();
            return Some((
                Compared::Actions,
                format!(
                    "the planning outcome differs: walk {:?}, eval {:?}",
                    error(walk_result),
                    error(result)
                ),
            ));
        }
        // A failed analysis leaves partial state the build never reads
        // (the walk's maps keep what a failed body wrote before failing):
        // only its error, warnings and console are compared.
        let failed = walk_result.is_err();
        if !failed && let Some(difference) = self.state_difference(walk) {
            return Some(difference);
        }
        let outcome = |result: &Result<root_checkpoint::Outcome<BuildPlan>, String>| match result {
            Err(error) => format!("error: {error}\n"),
            Ok(root_checkpoint::Outcome::AcceptedQuery(_)) => "accepted query\n".to_owned(),
            Ok(root_checkpoint::Outcome::Continue((nodes, requested, aspects, results))) => {
                let mut text = crate::request::format_action_graph(nodes, true);
                for (key, artifact) in requested {
                    text.push_str(&format!("requested {key} {:?}\n", artifact.real));
                }
                for (aspect, label, artifacts) in aspects {
                    let paths: Vec<_> = artifacts.iter().map(|artifact| &artifact.real).collect();
                    text.push_str(&format!("aspect {aspect} {label} {paths:?}\n"));
                }
                for (label, artifacts) in results {
                    let paths: Vec<_> = artifacts.iter().map(|artifact| &artifact.real).collect();
                    text.push_str(&format!("result {label} {paths:?}\n"));
                }
                text
            }
        };
        if let Some(difference) = text_difference(
            "the planned actions",
            &outcome(walk_result),
            &outcome(result),
        ) {
            return Some((Compared::Actions, difference));
        }
        let lists = |engine: &Engine| format!("{:#?}\n{:#?}\n", engine.warnings, engine.errors);
        if let Some(difference) =
            text_difference("the warnings and errors", &lists(walk), &lists(self))
        {
            return Some((Compared::Stderr, difference));
        }
        // The walk did the repository set's shared work (fetches, module
        // extensions, first `.bzl` evaluations): its console output is the
        // walk's only. Otherwise every write must agree.
        if let Some(difference) = text_difference(
            "the console writes",
            &console_text(walk_console, false),
            &console_text(eval_console, false),
        ) && text_difference(
            "",
            &console_text(walk_console, true),
            &console_text(eval_console, true),
        )
        .is_some()
        {
            return Some((Compared::Stderr, difference));
        }
        if !failed && self.tracking() {
            let index = |engine: &mut Engine| {
                engine.incremental_index(labels).map(|mut index| {
                    index.costs.clear();
                    format!("{index:#?}\n")
                })
            };
            let (walk_index, eval_index) = (index(walk), index(self));
            if let Some(difference) = text_difference(
                "the incremental indexes",
                walk_index.as_deref().unwrap_or(""),
                eval_index.as_deref().unwrap_or(""),
            ) {
                return Some((Compared::IncrementalIndex, difference));
            }
        }
        if let (Some(walk_capture), Some(eval_capture)) =
            (&walk.cquery_capture, &self.cquery_capture)
            && let Some(difference) = text_difference(
                "the cquery graphs",
                &walk_capture.verify_text(),
                &eval_capture.verify_text(),
            )
        {
            return Some((Compared::CqueryGraph, difference));
        }
        None
    }
}

impl Engine {
    /// A second engine in this engine's state before planning, over the
    /// same repository set (BAZEL_RS_EVAL_VERIFY: the walk plans on it).
    /// Per-build state is fresh; the capture switches (cquery/aquery) are
    /// set the same way, empty.
    pub(in crate::engine) fn fork_for_verify(&self) -> Engine {
        Engine {
            repositories: self.repositories.clone(),
            packages: self.packages.clone(),
            loading_phase_threads: self.loading_phase_threads,
            analyze_depth: self.analyze_depth,
            prefetch: None,
            prefetch_followed: self.prefetch_followed.clone(),
            prefetch_platforms: self.prefetch_platforms.clone(),
            skip_teardown: self.skip_teardown,
            cquery_capture: self.cquery_capture.as_ref().map(|_| Default::default()),
            cquery_files: self.cquery_files,
            cquery_build: self.cquery_build,
            cquery_transitions: self.cquery_transitions,
            cquery_fragments: self.cquery_fragments,
            aquery_capture: self.aquery_capture.as_ref().map(|_| Default::default()),
            console_owners: self.console_owners.clone(),
            emitted_bzl_prints: self.emitted_bzl_prints.clone(),
            declared_inputs: self.declared_inputs.clone(),
            source_targets: self.source_targets.clone(),
            rule_targets: self.rule_targets.clone(),
            analyzed_sources: self.analyzed_sources.clone(),
            loaded_functions: self.loaded_functions.clone(),
            generated: self.generated.clone(),
            group_nodes: self.group_nodes.clone(),
            group_budget: self.group_budget.clone(),
            configurations: self.configurations.clone(),
            missing_sources: self.missing_sources.clone(),
            warnings: self.warnings.clone(),
            platforms: self.platforms.clone(),
            resolving_platforms: self.resolving_platforms.clone(),
            module_execution_platforms: self.module_execution_platforms.clone(),
            empty_configuration_rules: self.empty_configuration_rules.clone(),
            registered_toolchains: self.registered_toolchains.clone(),
            computing_toolchains: self.computing_toolchains.clone(),
            root_checkpoint: None,
            toolchain_cycle_reports: self.toolchain_cycle_reports.clone(),
            registered_execution_platforms: self.registered_execution_platforms.clone(),
            computing_execution_platforms: self.computing_execution_platforms.clone(),
            execution_platform_cycle_reports: self.execution_platform_cycle_reports.clone(),
            exec_configurations: self.exec_configurations.clone(),
            flag_scopes: self.flag_scopes.clone(),
            exec_transitions: self.exec_transitions.clone(),
            transition_cache: self.transition_cache.clone(),
            transition_details: self.transition_details.clone(),
            edge_configs: self.edge_configs.clone(),
            rule_delegations: self.rule_delegations.clone(),
            requested_trimmed_root: self.requested_trimmed_root.clone(),
            toolchain_contexts: self.toolchain_contexts.clone(),
            exec_group_contexts: self.exec_group_contexts.clone(),
            group_exec_configurations: self.group_exec_configurations.clone(),
            action_executions: self.action_executions.clone(),
            action_planning: None,
            planning_plan: None,
            ahead: self.ahead,
            view_accesses: self.view_accesses.clone(),
            access_capture: Vec::new(),
            view_artifacts: self.view_artifacts.clone(),
            ahead_aborted: self.ahead_aborted,
            analysis_pool: None,
            lowering_pool: None,
            toolchain_implementations: self.toolchain_implementations.clone(),
            deferred_duplicates: self.deferred_duplicates.clone(),
            source_directories: self.source_directories.clone(),
            source_dir_probes: self.source_dir_probes.clone(),
            compact_repo_mapping: self.compact_repo_mapping,
            run_under: self.run_under.clone(),
            output_selection: self.output_selection.clone(),
            aspects: self.aspects.clone(),
            requested_aspects: self.requested_aspects.clone(),
            aspect_parameters: self.aspect_parameters.clone(),
            configured_rules: self.configured_rules.clone(),
            aspect_predicates: self.aspect_predicates.clone(),
            aspect_toolchains: self.aspect_toolchains.clone(),
            resolving_aspect_toolchains: self.resolving_aspect_toolchains,
            tests_requested: self.tests_requested,
            bep_groups: self.bep_groups.clone(),
            run_requests: self.run_requests.clone(),
            run_trees: self.run_trees,
            run_plans: self.run_plans.clone(),
            derive_recording: self.derive_recording,
            derive_roots: None,
            build_info_actions: self.build_info_actions.clone(),
            errors: self.errors.clone(),
            workspace_status: self.workspace_status.clone(),
            workspace_status_once: std::sync::OnceLock::new(),
            execute_build: self.execute_build,
            source_directories_checked: self.source_directories_checked,
            keep_going: self.keep_going,
            keep_going_state: Default::default(),
            keep_going_plan: None,
            explicit_targets: self.explicit_targets.clone(),
            skip_incompatible_explicit_targets: self.skip_incompatible_explicit_targets,
            skipped_incompatible: self.skipped_incompatible.clone(),
            incompatible_errors: self.incompatible_errors.clone(),
            pattern_load_errors: self.pattern_load_errors.clone(),
            incremental: self
                .incremental
                .as_ref()
                .map(|tracker| Box::new(tracker.fork())),
            aspect_attribute_configs: self.aspect_attribute_configs.clone(),
            aspect_attr_aspects: self.aspect_attr_aspects.clone(),
            phase2_analyzed: std::collections::BTreeSet::new(),
            eval_mode: Some(AnalysisMode::Walk),
            eval_verifying: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differences_name_the_first_line_and_key() {
        assert!(text_difference("x", "a\nb\n", "a\nb\n").is_none());
        let difference = text_difference("x", "a\nb\n", "a\nc\n").unwrap();
        assert!(difference.contains("line 2"), "{difference}");
        let walk = BTreeMap::from([(1, 10), (2, 20)]);
        let mut eval = walk.clone();
        assert!(map_difference("m", &walk, &eval).is_none());
        eval.insert(2, 21);
        assert!(
            map_difference("m", &walk, &eval)
                .unwrap()
                .contains("2 differs")
        );
        eval.remove(&2);
        assert!(
            map_difference("m", &walk, &eval)
                .unwrap()
                .contains("only in the walk")
        );
        assert_ne!(debug_digest(&"a"), debug_digest(&"b"));
    }
}
