//! `BAZEL_RS_EVAL_VERIFY_PHASE1=1` (S1, worker B): after the walk's phase
//! 1, the same requests are evaluated by the phase-1 nodes under the
//! inline driver, and their results are compared with the walk's: the
//! plan order (a minimal DFS replay of the transcripts: pass 1's order,
//! for this check only), the configured nodes, rule delegations,
//! configured rules, toolchain and exec-group contexts, exec and
//! edge configurations, deferred duplicates, the empty-configuration
//! rules and the toolchain-implementation claims. The first differences
//! are an error naming the key and the field. The walk's output is not
//! changed (the evaluation runs after it; its events are not emitted).
//!
//! Owner: worker B. Worker C's `BAZEL_RS_EVAL_VERIFY` (the dual-engine
//! oracle) supersedes it once pass 1 drives the walk.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use crate::configuration::ConfigId;
use crate::fast_hash::FxHashSet;
use crate::label::Label;

use super::super::super::{
    ConfiguredNodes, ConfiguredTargetKey, Engine, Plan, RuleKind, Target, forwards,
};
use std::sync::Arc;

use super::super::cycles::DfsCycles;
use super::super::graph::Graph;
use super::super::key::{NodeId, NodeKey};
use super::super::scheduler::{EvalConfig, Evaluator, NoRepositories};
use super::super::shared::EvalShared;
use super::super::transcript::Event;
use super::super::value::NodeValue;
use super::bodies::Phase1Bodies;
use super::eval_host::edge_ct_key;

/// Whether `BAZEL_RS_EVAL_VERIFY_PHASE1=1` asks for the comparison.
pub(crate) fn enabled() -> bool {
    // snapshot-unrecorded: a diagnostic switch; it never changes the plan.
    std::env::var_os("BAZEL_RS_EVAL_VERIFY_PHASE1").is_some_and(|value| value == "1")
}

/// The plan order and the empty-configuration rules of a DFS replay of
/// the transcripts from the requests' reaches (pass 1's order).
struct Replay<'g> {
    graph: &'g Graph,
    visited: FxHashSet<NodeId>,
    plan: Vec<ConfiguredTargetKey>,
    empty: BTreeSet<ConfiguredTargetKey>,
    /// The configured target of each key: the first variant reached that
    /// succeeded (pass 1's first-visit rule).
    canonical: BTreeMap<ConfiguredTargetKey, NodeId>,
    /// The first claim of each toolchain implementation.
    claims: BTreeMap<ConfiguredTargetKey, crate::label::Label>,
}

impl Replay<'_> {
    fn events(&mut self, owner: Option<&ConfiguredTargetKey>, events: &[Event]) {
        for event in events {
            match event {
                Event::Reach(id) | Event::MemoUse(id) => self.node(*id),
                Event::PlanPush => self.plan.extend(owner.cloned()),
                Event::ToolchainImpl(claim) => {
                    self.claims
                        .entry(claim.implementation.clone())
                        .or_insert_with(|| claim.exec_platform.clone());
                }
                _ => {}
            }
        }
    }

    fn node(&mut self, id: NodeId) {
        let graph = self.graph;
        if let NodeKey::Ct1(ct) = graph.key(id)
            && self.canonical.contains_key(&ct.key)
        {
            return;
        }
        if !self.visited.insert(id) {
            return;
        }
        if let NodeKey::Ct1(ct) = graph.key(id)
            && let Some(value) = graph.value(id)
            && let NodeValue::Ct1(value) = value.as_ref()
            && value.outcome.is_ok()
        {
            self.canonical.insert(ct.key.clone(), id);
        }
        if let NodeKey::Ct1(ct) = graph.key(id)
            && let Some(value) = graph.value(id)
            && let NodeValue::Ct1(value) = value.as_ref()
            && value
                .outcome
                .as_ref()
                .is_ok_and(|ok| ok.empty_configuration)
        {
            self.empty.insert(ct.key.clone());
        }
        if let Some(transcript) = graph.entry(id).transcript() {
            let owner = match graph.key(id) {
                NodeKey::Ct1(ct) => Some(ct.key.clone()),
                _ => None,
            };
            self.events(owner.as_ref(), &transcript.events);
        }
    }
}

/// Differences found, capped.
#[derive(Default)]
struct Diffs {
    lines: Vec<String>,
    count: usize,
}

impl Diffs {
    fn push(&mut self, line: String) {
        self.count += 1;
        if self.lines.len() < 20 {
            self.lines.push(line);
        }
    }

    fn compare<T: Debug>(
        &mut self,
        what: &str,
        key: &dyn Debug,
        walk: Option<&T>,
        eval: Option<&T>,
    ) {
        let (walk, eval) = (
            walk.map(|value| format!("{value:?}")),
            eval.map(|value| format!("{value:?}")),
        );
        if walk != eval {
            self.push(format!(
                "{what} of {key:?}: walk {} / eval {}",
                shorten(walk.as_deref()),
                shorten(eval.as_deref())
            ));
        }
    }
}

/// What phase 1 decides of a configured rule (its full Debug form holds
/// the rule class: too big to format per target).
fn rule_facts(rule: &crate::package::Rule) -> String {
    match &rule.custom {
        Some(custom) => format!(
            "{:?} {:?} {:?} {:?}",
            rule.srcs, custom.attrs, custom.split_dependencies, custom.setting_value
        ),
        None => format!("{:?} {:?}", rule.srcs, rule.native),
    }
}

fn shorten(text: Option<&str>) -> String {
    match text {
        None => "(none)".to_owned(),
        Some(text) if text.len() > 400 => format!("{}…", &text[..400]),
        Some(text) => text.to_owned(),
    }
}

impl Engine {
    /// Evaluate phase 1 of `labels` in `top` with the nodes and compare
    /// with the walk's results (`configured`, `plan` and the engine maps).
    pub(in crate::engine) fn verify_phase1(
        &mut self,
        labels: &[Label],
        top: ConfigId,
        configured: &ConfiguredNodes,
        plan: &Plan,
        walk_error: Option<&str>,
    ) -> Result<(), String> {
        if self.cquery_capture.is_some() || self.keep_going || !self.requested_aspects.is_empty() {
            // S1: cquery captures, keep-going bookkeeping and top-level
            // aspects are not evaluated (S4).
            return Ok(());
        }
        let started = std::time::Instant::now();
        // The evaluation must not leave traces in the walk's state.
        let warnings = self.warnings.len();
        let incremental = self.incremental.take();
        let shared = EvalShared::from_engine(self);
        // The inline driver recurses as deep as the graph: a big stack.
        let result = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("eval-verify".to_owned())
                .stack_size(1 << 30)
                .spawn_scoped(scope, || {
                    verify_with(self, &shared, labels, top, configured, plan, walk_error)
                })
                .map_err(|error| error.to_string())?
                .join()
                .map_err(|_| "BAZEL_RS_EVAL_VERIFY_PHASE1: the evaluation panicked".to_owned())?
        });
        shared.restore(self);
        self.incremental = incremental;
        self.warnings.truncate(warnings);
        let (nodes, diffs) = result?;
        if diffs.count > 0 {
            return Err(format!(
                "BAZEL_RS_EVAL_VERIFY_PHASE1: {} difference(s) between the walk and the phase-1 nodes ({nodes} nodes, {:.2} s):\n{}",
                diffs.count,
                started.elapsed().as_secs_f64(),
                diffs.lines.join("\n")
            ));
        }
        // The success line goes to BAZEL_RS_EVAL_VERIFY_PHASE1_LOG (stderr
        // stays the walk's).
        // snapshot-unrecorded: a diagnostic log path; it never changes the plan.
        if let Some(log) = std::env::var_os("BAZEL_RS_EVAL_VERIFY_PHASE1_LOG") {
            use std::io::Write;
            // snapshot-unrecorded: a diagnostic log, written after the comparison.
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
            {
                let _ = writeln!(
                    file,
                    "identical: {} configured targets, {} plan entries, {nodes} nodes, {:.2} s, {:?}",
                    configured.len(),
                    plan.len(),
                    started.elapsed().as_secs_f64(),
                    labels.first()
                );
            }
        }
        Ok(())
    }
}

fn verify_with(
    engine: &mut Engine,
    shared: &EvalShared,
    labels: &[Label],
    top: ConfigId,
    configured: &ConfiguredNodes,
    plan: &Plan,
    walk_error: Option<&str>,
) -> Result<(usize, Diffs), String> {
    let mut diffs = Diffs::default();
    let bodies = Phase1Bodies::new(shared, engine, false, false);
    let evaluator = Evaluator::new(
        &bodies,
        EvalConfig {
            threads: 1,
            keep_going: false,
            jitter_micros: 0,
            ..EvalConfig::default()
        },
    );
    let evaluate = |key: &NodeKey| -> Arc<NodeValue> {
        evaluator.evaluate(
            std::slice::from_ref(key),
            &mut NoRepositories,
            &mut DfsCycles::default(),
        );
        evaluator.value(key).cloned().unwrap_or_else(|| {
            Arc::new(NodeValue::Src(Err(format!(
                "internal error: {key:?} did not finish"
            ))))
        })
    };
    let computed =
        |evaluator: &Evaluator<'_, Phase1Bodies<'_, '_>>| evaluator.stats().done as usize;
    let mut roots = Vec::new();
    // Engine::plan_build: a non-default target platform first.
    let top_configuration = shared
        .configurations
        .lock()
        .unwrap()
        .top_configuration()
        .clone();
    let target = top_configuration.native.target_platform().clone();
    if target != crate::configuration::default_host_platform()
        && target != top_configuration.native.host_platform
    {
        let key = NodeKey::Platform(ConfiguredTargetKey::new(target.clone(), top));
        roots.push(evaluator.graph().intern(&key));
        if let NodeValue::Platform(Err(error)) = evaluate(&key).as_ref() {
            let error = error.clone().target_text();
            if walk_error != Some(error.as_str()) {
                diffs.push(format!(
                    "the target platform: walk {walk_error:?} / eval {error:?}"
                ));
            }
            return Ok((computed(&evaluator), diffs));
        }
    }
    for label in labels {
        // Engine::analyze_requested, then the root edge of Phase1::analyze.
        let looked_up = bodies.coord().engine.target(label);
        let trimmed = match &looked_up {
            Ok(Target::Rule(rule))
                if !forwards(rule) && !matches!(rule.kind, RuleKind::PackageGroup(_)) =>
            {
                Some(ConfiguredTargetKey::new(rule.label.clone(), top))
            }
            _ => None,
        };
        let root = match looked_up {
            Err(error) => Err(error),
            Ok(Target::Source(_)) => Ok(NodeKey::Src(label.clone())),
            Ok(Target::Rule(rule) | Target::Output(rule, _)) => {
                super::super::super::check_macro_namespace(&rule, label).map(|()| {
                    let no_config = shared
                        .configurations
                        .lock()
                        .unwrap()
                        .get(top)
                        .native
                        .no_config;
                    let key = ConfiguredTargetKey::new(rule.label.clone(), top);
                    NodeKey::Ct1(edge_ct_key(no_config, &rule, key, false, None))
                })
            }
        };
        let analyzed = root.and_then(|root| {
            roots.push(evaluator.graph().intern(&root));
            let value = evaluate(&root);
            match value.as_ref() {
                NodeValue::Src(result) => result.clone(),
                NodeValue::Ct1(value) => value.outcome.as_ref().map(|_| ()).map_err(Clone::clone),
                _ => Err("internal error: a root with another value".to_owned()),
            }
        });
        let analyzed = analyzed.map_err(|error| {
            crate::engine::eval::linearize::driver::render_chains(shared, &error, trimmed.as_ref())
        });
        // Engine::analyze_requested's report of an aborted analysis.
        let analyzed = analyzed.map_err(|error| {
            match error.split_once(super::super::super::ANALYSIS_ABORTED) {
                Some((_, message)) => format!(
                    "Analysis of target '{}' failed; build aborted: {message}",
                    crate::rule_declarations::canonical_form(label)
                ),
                None => error,
            }
        });
        match (analyzed, walk_error) {
            (Ok(()), None) => {}
            (Ok(()), Some(walk)) => {
                // The walk's failure may be the requested target's own
                // compatibility (after its analysis).
                let compatible = bodies
                    .coord()
                    .engine
                    .check_requested_compatibility(configured, label, top);
                if let Err(error) = compatible {
                    if error != walk {
                        diffs.push(format!("{label}: walk {walk:?} / compatibility {error:?}"));
                    }
                    return Ok((computed(&evaluator), diffs));
                }
            }
            (Err(error), walk) => {
                // A config_setting's Starlark flag_values keys need the
                // planned implementations (the walk's flush runs phase 2;
                // the CsMatch node with CT2 dependencies is S2): an error
                // of that phase 2 is not comparable here.
                // Which registered target a toolchain / execution platform
                // cycle report names is pass 1's decision (the walk
                // re-analyzes a failed target in each registration's
                // chain; the graph memoizes the first failure; §7:
                // `toolchain_cycle_reports` is linearizer-only).
                let cycle_report = |text: &str| {
                    text.starts_with("Misconfigured toolchains: ")
                        || text.starts_with("Misconfigured execution platforms: ")
                };
                let pass1 = walk.is_some_and(cycle_report) && cycle_report(&error);
                if walk != Some(error.as_str())
                    && !bodies.saw_flush.load(std::sync::atomic::Ordering::Relaxed)
                    && !pass1
                {
                    diffs.push(format!("{label}: walk {walk:?} / eval {error:?}"));
                }
                return Ok((computed(&evaluator), diffs));
            }
        }
    }
    if let Some(walk) = walk_error {
        diffs.push(format!("the walk failed, the nodes did not: {walk:?}"));
        return Ok((computed(&evaluator), diffs));
    }
    crate::perf_mark("plan.verify_phase1_evaluated");
    let graph = evaluator.graph();
    let mut replay = Replay {
        graph,
        visited: FxHashSet::default(),
        plan: Vec::new(),
        empty: BTreeSet::new(),
        canonical: BTreeMap::new(),
        claims: BTreeMap::new(),
    };
    let roots = roots.into_iter().map(Event::Reach).collect::<Vec<_>>();
    replay.events(None, &roots);
    let (eval_plan, eval_empty, canonical, claims) =
        (replay.plan, replay.empty, replay.canonical, replay.claims);
    // The plan, keys and order.
    // Entries a deferred target platform's analysis in phase 2 put into
    // the walk's plan (phase 2, S3) are compared as configured nodes only.
    let coord = bodies.coord();
    let phase2 = &coord.engine.phase2_analyzed;
    let walk_plan = plan
        .iter()
        .map(|(key, _)| key.clone())
        .filter(|key| !phase2.contains(key))
        .collect::<Vec<_>>();
    let eval_plan = eval_plan
        .into_iter()
        .filter(|key| !phase2.contains(key))
        .collect::<Vec<_>>();
    if walk_plan != eval_plan {
        let first = walk_plan
            .iter()
            .zip(&eval_plan)
            .position(|(walk, eval)| walk != eval)
            .unwrap_or(walk_plan.len().min(eval_plan.len()));
        diffs.push(format!(
            "plan: walk {} entries / eval {} entries; first difference at {first}: walk {:?} / eval {:?}",
            walk_plan.len(),
            eval_plan.len(),
            walk_plan.get(first),
            eval_plan.get(first)
        ));
    }
    // The CT1 values by ConfiguredTargetKey (first variants).
    let mut eval_ok = BTreeMap::new();
    for (key, &id) in &canonical {
        if let Some(value) = graph.value(id)
            && let NodeValue::Ct1(value) = value.as_ref()
            && value.outcome.is_ok()
        {
            eval_ok.insert(key.clone(), std::sync::Arc::clone(value));
        }
    }
    let engine = &*coord.engine;
    let walk_keys = configured.keys().cloned().collect::<BTreeSet<_>>();
    let eval_keys = eval_ok.keys().cloned().collect::<BTreeSet<_>>();
    for missing in walk_keys.difference(&eval_keys) {
        diffs.push(format!("configured: {missing:?} only in the walk"));
    }
    for extra in eval_keys.difference(&walk_keys) {
        diffs.push(format!("configured: {extra:?} only in the nodes"));
    }
    for (key, value) in &eval_ok {
        let Ok(ok) = &value.outcome else { continue };
        diffs.compare("configured node", key, configured.get(key), Some(&ok.node));
        diffs.compare(
            "rule delegation",
            key,
            engine.rule_delegations.get(key),
            ok.delegate.as_ref(),
        );
        if ok.delegate.is_none() && (ok.rule.custom.is_some() || ok.node.incompatible.is_some()) {
            diffs.compare(
                "configured rule",
                key,
                engine
                    .configured_rules
                    .get(key)
                    .map(|rule| rule_facts(rule))
                    .as_ref(),
                Some(&rule_facts(&ok.rule)),
            );
        }
        diffs.compare(
            "toolchain context",
            key,
            engine.toolchain_contexts.get(key),
            ok.toolchain_context.as_ref(),
        );
        diffs.compare(
            "exec groups",
            key,
            engine.exec_group_contexts.get(key),
            ok.exec_groups.as_ref(),
        );
        diffs.compare(
            "exec configuration",
            key,
            engine.exec_configurations.get(key),
            ok.exec_config.as_ref(),
        );
        let walk_groups = engine
            .group_exec_configurations
            .range((key.clone(), String::new())..)
            .take_while(|((owner, _), _)| owner == key)
            .map(|((_, group), config)| (group.clone(), *config))
            .collect::<BTreeMap<_, _>>();
        diffs.compare(
            "group exec configurations",
            key,
            Some(&walk_groups),
            Some(&ok.group_exec_configs),
        );
        diffs.compare(
            "edge configurations",
            key,
            engine.edge_configs.get(key),
            ok.edge_configs.as_ref(),
        );
        diffs.compare(
            "deferred duplicates",
            key,
            engine.deferred_duplicates.get(key),
            ok.deferred_duplicates.as_ref(),
        );
    }
    let walk_empty = engine
        .empty_configuration_rules
        .iter()
        .filter(|key| configured.contains_key(*key))
        .cloned()
        .collect::<BTreeSet<_>>();
    if walk_empty != eval_empty {
        diffs.push(format!(
            "empty-configuration rules: walk {} / eval {}; walk only {:?}; eval only {:?}",
            walk_empty.len(),
            eval_empty.len(),
            walk_empty
                .difference(&eval_empty)
                .take(5)
                .collect::<Vec<_>>(),
            eval_empty
                .difference(&walk_empty)
                .take(5)
                .collect::<Vec<_>>()
        ));
    }
    if engine.toolchain_implementations != claims {
        diffs.push(format!(
            "toolchain implementations: walk {} / eval {}",
            engine.toolchain_implementations.len(),
            claims.len()
        ));
    }
    let nodes = graph.len();
    drop(coord);
    Ok((nodes, diffs))
}
