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
use super::super::key::{NodeId, NodeKey};
use super::super::shared::EvalShared;
use super::super::transcript::Event;
use super::super::value::NodeValue;
use super::eval_host::EvalHost;
use super::host::Host;
use super::inline::Inline;

/// Whether `BAZEL_RS_EVAL_VERIFY_PHASE1=1` asks for the comparison.
pub(crate) fn enabled() -> bool {
    // snapshot-unrecorded: a diagnostic switch; it never changes the plan.
    std::env::var_os("BAZEL_RS_EVAL_VERIFY_PHASE1").is_some_and(|value| value == "1")
}

/// The plan order and the empty-configuration rules of a DFS replay of
/// the transcripts from the requests' events (pass 1's order).
struct Replay<'d, 'a> {
    driver: &'d Inline<'a>,
    visited: FxHashSet<NodeId>,
    plan: Vec<ConfiguredTargetKey>,
    empty: BTreeSet<ConfiguredTargetKey>,
}

impl Replay<'_, '_> {
    fn events(&mut self, events: &[Event]) {
        for event in events {
            match event {
                Event::Reach(id) | Event::MemoUse(id) => self.node(*id),
                Event::PlanPush(ct) => self.plan.push(ct.key.clone()),
                _ => {}
            }
        }
    }

    fn node(&mut self, id: NodeId) {
        if !self.visited.insert(id) {
            return;
        }
        let key = self.driver.interner.key(id).clone();
        if let NodeKey::Ct1(ct) = &key
            && let Some(value) = self.driver.value(id)
            && let NodeValue::Ct1(value) = value.as_ref()
        {
            if value
                .outcome
                .as_ref()
                .is_ok_and(|ok| ok.empty_configuration)
            {
                self.empty.insert(ct.key.clone());
            }
            let events = value.transcript.events.clone();
            self.events(&events);
            return;
        }
        if let Some(transcript) = self.driver.transcript(id) {
            let events = transcript.events.clone();
            self.events(&events);
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
        let result = verify_with(self, &shared, labels, top, configured, plan);
        shared.restore(self);
        self.incremental = incremental;
        self.warnings.truncate(warnings);
        let (nodes, diffs) = result?;
        if diffs.count > 0 {
            return Err(format!(
                "BAZEL_RS_EVAL_VERIFY_PHASE1: {} difference(s) between the walk and the phase-1 nodes:\n{}",
                diffs.count,
                diffs.lines.join("\n")
            ));
        }
        // The success line goes to BAZEL_RS_EVAL_VERIFY_PHASE1_LOG (stderr
        // stays the walk's).
        // snapshot-unrecorded: a diagnostic log path; it never changes the plan.
        if let Some(log) = std::env::var_os("BAZEL_RS_EVAL_VERIFY_PHASE1_LOG") {
            use std::io::Write;
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
) -> Result<(usize, Diffs), String> {
    let mut driver = Inline::new(engine, shared);
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
        let mut host = EvalHost::new(shared, &mut driver, None, false);
        host.platform_info(&target, top)
            .map_err(|error| error.target_text())?;
    }
    for label in labels {
        // Engine::analyze_requested.
        let trimmed = match driver.engine_mut_target(label) {
            Ok(Target::Rule(rule))
                if !forwards(&rule) && !matches!(rule.kind, RuleKind::PackageGroup(_)) =>
            {
                Some(ConfiguredTargetKey::new(rule.label.clone(), top))
            }
            _ => None,
        };
        driver.first.requested_trimmed_root = trimmed;
        let mut host = EvalHost::new(shared, &mut driver, None, false);
        let analyzed = host.analyze(label, top);
        driver.first.requested_trimmed_root = None;
        analyzed.map_err(|error| {
            format!("BAZEL_RS_EVAL_VERIFY_PHASE1: the nodes failed where the walk did not: {error}")
        })?;
    }
    let mut replay = Replay {
        driver: &driver,
        visited: FxHashSet::default(),
        plan: Vec::new(),
        empty: BTreeSet::new(),
    };
    let roots = driver.root_events.clone();
    replay.events(&roots);
    let (eval_plan, eval_empty) = (replay.plan, replay.empty);
    let mut diffs = Diffs::default();
    // The plan, keys and order.
    // Entries a deferred target platform's analysis in phase 2 put into
    // the walk's plan (phase 2, S3) are compared as configured nodes only.
    let phase2 = &driver.engine_ref().phase2_analyzed;
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
    for (key, ct) in &driver.first.canonical {
        let Some(id) = driver.interner.get(&NodeKey::Ct1(ct.clone())) else {
            continue;
        };
        if let Some(value) = driver.value(id)
            && let NodeValue::Ct1(value) = value.as_ref()
            && value.outcome.is_ok()
        {
            eval_ok.insert(key.clone(), std::sync::Arc::clone(value));
        }
    }
    let engine = driver.engine_ref();
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
                engine.configured_rules.get(key).map(|rule| &**rule),
                Some(&*ok.rule),
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
    if engine.toolchain_implementations != driver.first.claims {
        diffs.push(format!(
            "toolchain implementations: walk {} / eval {}",
            engine.toolchain_implementations.len(),
            driver.first.claims.len()
        ));
    }
    Ok((driver.computed, diffs))
}

impl Inline<'_> {
    /// A coordinator lookup outside any node (the requests' trimming).
    fn engine_mut_target(&mut self, label: &Label) -> Result<Target, String> {
        use super::eval_host::Phase1Cx;
        self.engine().target(label)
    }
}
