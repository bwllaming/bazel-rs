//! T3 aspect prefetch: the attribute-aspect applications of an entry at its
//! turn (rules_rust's clippy and rustfmt aspects over the Rust dependency
//! tree) analyzed on the analysis workers before the turn's prepare.
//!
//! At the turn of a Starlark rule with attribute aspects, the coordinator
//! walks its aspect-applied edges *dry* first ([`Mode::Dry`]): the same
//! recursion as the prepare's (`Engine::plan_aspect_dependency`), which
//! builds each application's inputs (`analysis::AspectApplication`) but,
//! instead of analyzing it, queues it as a job and reports it pending
//! ([`PENDING`]); the dependencies above it are pending too, the others are
//! walked on. The jobs run on the workers (and the cursor); as each one is
//! analyzed, the dependencies waiting for it are walked again ([`Waiter`]):
//! an analyzed application's providers stand in for its result, a
//! dependency all of whose applications are analyzed is walked once, a
//! pending one returns at once. Nothing is committed while dry: no aspect
//! memo, no action, no print. Once nothing is pending, the prepare runs as
//! before ([`Mode::Replay`]): each application takes its analysis from the
//! table instead of analyzing (its inputs are built from the same planning
//! state and the same analyses, so they are the dry walk's) and commits in
//! the sequential order. `BAZEL_RS_VERIFY_DELTA=1` analyzes again there and
//! compares.
//!
//! An application whose analysis failed, panicked or had effects ends the
//! dry walk ([`STOP`]): the prepare meets it in order (an error reported, a
//! panic raised, the effects' analysis redone at its point).

use super::*;
use crate::aspects::AspectApplicationKey;
use std::sync::atomic::Ordering;

/// A dry walk's dependency waiting for an analysis.
pub(in crate::engine) const PENDING: &str = "\u{1}aspect-prefetch-pending";
/// A dry walk met what only the prepare may decide.
pub(in crate::engine) const STOP: &str = "\u{1}aspect-prefetch-stop";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Dry,
    Replay,
}

/// One application's analysis on a worker.
struct Job {
    key: AspectApplicationKey,
    application: crate::analysis::AspectApplication,
    output_root: String,
    action_owner: String,
    action_env: Arc<crate::action_env::ActionEnvironment>,
    runfiles: crate::runfiles_tree::RunfilesOptions,
    loaded_functions: Option<Arc<crate::analysis_runtime::LoadedFunctions>>,
    run_under: Option<Arc<crate::analysis::RunUnderPrerequisite>>,
}

enum Outcome {
    Analyzed {
        analyzed: Box<crate::analysis::AnalysisResult>,
        printed: Vec<String>,
    },
    /// Failed, panicked or had effects: the prepare analyzes it itself.
    Unusable,
}

/// An aspect-applied dependency of the dry walk: label, edge configuration
/// and the aspects' descriptors.
type Walked = (Label, ConfigId, Vec<String>);

/// What a pending dependency waits for.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Waiter {
    Application(AspectApplicationKey),
    Dependency(Walked),
}

struct Prefetch {
    mode: Mode,
    /// The jobs queued and not taken yet, and those queued or running.
    jobs: Vec<Job>,
    queued: std::collections::HashSet<AspectApplicationKey>,
    results: HashMap<AspectApplicationKey, Outcome>,
    /// The providers of the applications the dry walk planned.
    dry: HashMap<AspectApplicationKey, AspectProviders>,
    /// The dependencies walked: planned (all their applications
    /// analyzed), or pending (walked again once what they wait for is).
    planned: HashMap<Walked, (DependencyTarget, AspectApplications)>,
    pending: std::collections::HashSet<Walked>,
    /// Who waits for what, and the walk's open dependencies (innermost
    /// last).
    waiting: HashMap<Waiter, Vec<Walked>>,
    /// What each pending dependency still waits for: walked again once
    /// nothing.
    outstanding: HashMap<Walked, std::collections::HashSet<Waiter>>,
    stack: Vec<Walked>,
    /// The arguments of each pending dependency, to walk it again.
    arguments: HashMap<Walked, Vec<Arc<crate::aspects::AspectDefinition>>>,
}

impl Prefetch {
    /// The innermost open dependency waits for `waiter`.
    fn wait(&mut self, waiter: Waiter) {
        if let Some(parent) = self.stack.last().cloned() {
            let waiting = self.waiting.entry(waiter.clone()).or_default();
            if !waiting.contains(&parent) {
                waiting.push(parent.clone());
            }
            self.outstanding.entry(parent).or_default().insert(waiter);
        }
    }

    /// `waiter` is done: the dependencies that waited for nothing else.
    fn done(&mut self, waiter: Waiter, again: &mut Vec<Walked>) {
        for walked in self.waiting.remove(&waiter).unwrap_or_default() {
            let outstanding = self.outstanding.entry(walked.clone()).or_default();
            outstanding.remove(&waiter);
            if outstanding.is_empty() {
                again.push(walked);
            }
        }
    }
}

thread_local! {
    static PREFETCH: std::cell::RefCell<Option<Prefetch>> = const { std::cell::RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Prefetch) -> R) -> Option<R> {
    PREFETCH.with(|cell| cell.borrow_mut().as_mut().map(f))
}

/// BAZEL_RS_TIMING (`PERF-PARALLEL aspects`).
static STATS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
const STAT_NAMES: [&str; 10] = [
    "turns", "walks", "rewalks", "analyzed", "used", "redone", "dry_us", "wait_us", "inline",
    "skipped",
];

/// Each aspect's analyses so far (descriptor: µs, count): an aspect whose
/// analyses are cheap is analyzed where the dry walk meets it, and a turn
/// whose aspects are all cheap is not prefetched (the dry walk's planning
/// would cost more than its analyses).
static COSTS: std::sync::Mutex<BTreeMap<String, (u64, u64)>> =
    std::sync::Mutex::new(BTreeMap::new());

/// An analysis cheaper than this (on average) is not worth a job.
const CHEAP_US: u64 = 1000;

fn note_cost(aspect: &str, micros: u64) {
    let mut costs = COSTS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = costs.entry(aspect.to_owned()).or_default();
    entry.0 += micros;
    entry.1 += 1;
}

/// Whether `aspect`'s analyses are known to be cheap.
fn cheap(aspect: &str) -> bool {
    COSTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(aspect)
        .is_some_and(|(micros, count)| *count >= 4 && *micros < CHEAP_US * *count)
}

fn stat(index: usize, value: u64) {
    STATS[index].fetch_add(value, Ordering::Relaxed);
}

pub(super) fn report() {
    if STATS[0].load(Ordering::Relaxed) == 0 {
        return;
    }
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL aspects {}",
        STAT_NAMES
            .iter()
            .zip(&STATS)
            .map(|(name, value)| format!("{name}={}", value.load(Ordering::Relaxed)))
            .collect::<Vec<_>>()
            .join(" ")
    ));
}

/// `BAZEL_RS_T3_ASPECTS=0`: the aspect applications at their turn analyzed
/// there, as before.
fn enabled() -> bool {
    static ENABLED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling switch; the output is the same either way.
        std::env::var("BAZEL_RS_T3_ASPECTS").map_or(true, |value| value != "0")
    });
    *ENABLED
}

/// Whether a dry walk is running.
pub(in crate::engine) fn dry() -> bool {
    with(|prefetch| prefetch.mode == Mode::Dry).unwrap_or(false)
}

/// An application analyzed for the dry walk: its providers.
pub(in crate::engine) fn dry_providers(key: &AspectApplicationKey) -> Option<AspectProviders> {
    with(|prefetch| {
        if prefetch.mode != Mode::Dry {
            return None;
        }
        if let Some(providers) = prefetch.dry.get(key) {
            return Some(providers.clone());
        }
        match prefetch.results.get(key) {
            Some(Outcome::Analyzed { analyzed, .. }) => Some(analyzed.providers.clone()),
            _ => None,
        }
    })
    .flatten()
}

/// Note the providers of an application the dry walk planned.
pub(in crate::engine) fn note_dry(key: &AspectApplicationKey, providers: &AspectProviders) {
    with(|prefetch| {
        prefetch.dry.insert(key.clone(), providers.clone());
    });
}

/// What [`enter_walk`] found.
pub(in crate::engine) enum Enter {
    /// Planned already, or pending (and now waited for).
    Known(Result<(DependencyTarget, AspectApplications), String>),
    /// To walk (then [`leave_walk`]).
    Walk(Walked),
}

/// The dry walk enters an aspect-applied dependency.
pub(in crate::engine) fn enter_walk(
    label: &Label,
    edge_config: ConfigId,
    aspects: &[Arc<crate::aspects::AspectDefinition>],
) -> Enter {
    let walked: Walked = (
        label.clone(),
        edge_config,
        aspects
            .iter()
            .map(|aspect| aspect.descriptor.clone())
            .collect(),
    );
    with(|prefetch| {
        if let Some(planned) = prefetch.planned.get(&walked) {
            return Enter::Known(Ok(planned.clone()));
        }
        if prefetch.pending.contains(&walked) {
            prefetch.wait(Waiter::Dependency(walked));
            return Enter::Known(Err(PENDING.to_owned()));
        }
        prefetch
            .arguments
            .entry(walked.clone())
            .or_insert_with(|| aspects.to_vec());
        prefetch.outstanding.remove(&walked);
        prefetch.stack.push(walked.clone());
        Enter::Walk(walked)
    })
    .unwrap_or_else(|| Enter::Known(Err(STOP.to_owned())))
}

/// The dry walk leaves a dependency it walked.
pub(in crate::engine) fn leave_walk(
    walked: Walked,
    planned: &Result<(DependencyTarget, AspectApplications), String>,
) {
    with(|prefetch| {
        prefetch.stack.pop();
        match planned {
            Ok(planned) => {
                prefetch.pending.remove(&walked);
                prefetch.planned.insert(walked, planned.clone());
            }
            Err(error) if error == PENDING => {
                prefetch.pending.insert(walked.clone());
                prefetch.wait(Waiter::Dependency(walked));
            }
            Err(_) => {}
        }
    });
}

/// What [`analysis`] decided for an application.
pub(in crate::engine) enum Analysis {
    /// Analyze it here (no prefetch, or its prefetch is not usable).
    Here(Box<crate::analysis::AspectApplication>),
    /// Analyzed off the turn.
    Done {
        analyzed: Box<crate::analysis::AnalysisResult>,
        printed: Vec<String>,
    },
    /// The dry walk: its providers (analyzed), or [`PENDING`] / [`STOP`].
    Dry(Result<AspectProviders, String>),
}

/// The analysis of the application `key` with inputs `application`, as the
/// mode decides: dry, queued (pending) or its providers; replaying, the
/// analysis made off the turn.
pub(in crate::engine) fn analysis(
    key: &AspectApplicationKey,
    application: crate::analysis::AspectApplication,
    outputs: &crate::analysis::ConfiguredOutputs<'_>,
) -> Analysis {
    PREFETCH.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(prefetch) = cell.as_mut() else {
            return Analysis::Here(Box::new(application));
        };
        match prefetch.mode {
            Mode::Dry => Analysis::Dry(match prefetch.results.get(key) {
                Some(Outcome::Analyzed { analyzed, .. }) => Ok(analyzed.providers.clone()),
                Some(Outcome::Unusable) => Err(STOP.to_owned()),
                None if cheap(&key.aspect) => {
                    // Cheap: analyzed here, now (kept for the replay).
                    stat(8, 1);
                    let outcome = analyze(Job {
                        key: key.clone(),
                        application,
                        output_root: outputs.output_root.to_owned(),
                        action_owner: outputs.action_owner.to_owned(),
                        action_env: outputs.action_env.clone(),
                        runfiles: outputs.runfiles,
                        loaded_functions: outputs.loaded_functions.cloned(),
                        run_under: outputs.run_under.clone(),
                    });
                    let providers = match &outcome {
                        Outcome::Analyzed { analyzed, .. } => Ok(analyzed.providers.clone()),
                        Outcome::Unusable => Err(STOP.to_owned()),
                    };
                    prefetch.results.insert(key.clone(), outcome);
                    providers
                }
                None => {
                    if prefetch.queued.insert(key.clone()) {
                        prefetch.jobs.push(Job {
                            key: key.clone(),
                            application,
                            output_root: outputs.output_root.to_owned(),
                            action_owner: outputs.action_owner.to_owned(),
                            action_env: outputs.action_env.clone(),
                            runfiles: outputs.runfiles,
                            loaded_functions: outputs.loaded_functions.cloned(),
                            run_under: outputs.run_under.clone(),
                        });
                    }
                    prefetch.wait(Waiter::Application(key.clone()));
                    Err(PENDING.to_owned())
                }
            }),
            Mode::Replay => match prefetch.results.remove(key) {
                Some(Outcome::Analyzed { analyzed, printed }) => {
                    stat(4, 1);
                    if delta::verify_delta() {
                        verify(key, &application, outputs, &analyzed, &printed);
                    }
                    Analysis::Done { analyzed, printed }
                }
                Some(Outcome::Unusable) => {
                    stat(5, 1);
                    Analysis::Here(Box::new(application))
                }
                None => Analysis::Here(Box::new(application)),
            },
        }
    })
}

/// BAZEL_RS_VERIFY_DELTA: the analysis again, here, compared.
fn verify(
    key: &AspectApplicationKey,
    application: &crate::analysis::AspectApplication,
    outputs: &crate::analysis::ConfiguredOutputs<'_>,
    analyzed: &crate::analysis::AnalysisResult,
    printed: &[String],
) {
    let (again, printed_again) = crate::analysis::analyze_aspect_buffered(application, outputs);
    let again = again.unwrap_or_else(|error| {
        panic!("internal error: aspect prefetch verify: {key:?} failed here: {error}")
    });
    let actions = |result: &crate::analysis::AnalysisResult| {
        result
            .actions
            .iter()
            .map(|action| action.action.id.clone())
            .collect::<Vec<_>>()
    };
    let same = actions(&again) == actions(analyzed)
        && again.actions.iter().zip(&analyzed.actions).all(|(a, b)| {
            a.key.sharing(&b.key) == crate::shared_actions::Sharing::Shared
                && a.inputs == b.inputs
                && a.outputs == b.outputs
        })
        && again.providers.keys().eq(analyzed.providers.keys())
        && again.provider_keys == analyzed.provider_keys
        && again.files == analyzed.files
        && again.declared == analyzed.declared
        && printed_again == printed;
    assert!(
        same,
        "internal error: aspect prefetch verify: the analysis of {key:?} off the turn differs from the turn's"
    );
}

/// The jobs in flight: queued for the workers (and the cursor), and their
/// outcomes.
struct Pool {
    queue: std::sync::Mutex<std::collections::VecDeque<Job>>,
    done: std::sync::Mutex<Vec<(AspectApplicationKey, Outcome)>>,
    finished: std::sync::Condvar,
}

impl Pool {
    /// Run one queued job; whether there was one.
    fn run_one(&self) -> bool {
        let Some(job) = self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        else {
            return false;
        };
        let key = job.key.clone();
        let outcome = analyze(job);
        self.done
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((key, outcome));
        self.finished.notify_all();
        true
    }
}

impl Engine {
    /// T3: analyze the attribute-aspect applications of `rule`'s edges on
    /// the workers before its prepare at its turn (module docs); the
    /// prepare then replays them. Ends with [`end_prefetch`].
    pub(super) fn prefetch_aspects(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Rule,
        submit: &dyn Fn(Box<dyn FnOnce() + Send>),
    ) {
        if !enabled() || self.semantic_only() {
            return;
        }
        if let Some(definitions) = rule.custom.as_ref().map(|custom| {
            custom
                .schema
                .values()
                .flat_map(|spec| spec.declaration.aspects.iter().cloned())
                .collect::<Vec<_>>()
        }) && let Ok(aspects) = self.attribute_aspects(rule, &definitions)
            && aspects.iter().all(|aspect| cheap(&aspect.descriptor))
        {
            stat(9, 1);
            return;
        }
        stat(0, 1);
        PREFETCH.with(|cell| {
            *cell.borrow_mut() = Some(Prefetch {
                mode: Mode::Dry,
                jobs: Vec::new(),
                queued: Default::default(),
                results: HashMap::new(),
                dry: HashMap::new(),
                planned: HashMap::new(),
                pending: Default::default(),
                waiting: HashMap::new(),
                outstanding: HashMap::new(),
                stack: Vec::new(),
                arguments: HashMap::new(),
            });
        });
        let pool = Arc::new(Pool {
            queue: Default::default(),
            done: Default::default(),
            finished: std::sync::Condvar::new(),
        });
        let mut in_flight = 0usize;
        let started = std::time::Instant::now();
        let mut stopped = self
            .dry_aspect_edges(state, configured, key, rule)
            .is_err_and(|error| error != PENDING);
        stat(1, 1);
        let mut dry_us = started.elapsed().as_micros() as u64;
        let mut wait_us = 0;
        loop {
            let jobs = with(|prefetch| std::mem::take(&mut prefetch.jobs)).unwrap_or_default();
            if !stopped {
                in_flight += jobs.len();
                stat(3, jobs.len() as u64);
                let count = jobs.len();
                pool.queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(jobs);
                for _ in 0..count {
                    let pool = Arc::clone(&pool);
                    submit(Box::new(move || {
                        pool.run_one();
                    }));
                }
            }
            if in_flight == 0 {
                break;
            }
            // Wait for an analysis (running queued ones meanwhile).
            let started = std::time::Instant::now();
            let finished = loop {
                let finished =
                    std::mem::take(&mut *pool.done.lock().unwrap_or_else(|e| e.into_inner()));
                if !finished.is_empty() {
                    break finished;
                }
                if pool.run_one() {
                    continue;
                }
                let done = pool.done.lock().unwrap_or_else(|e| e.into_inner());
                if done.is_empty() {
                    let _ = pool
                        .finished
                        .wait_timeout(done, std::time::Duration::from_millis(50))
                        .unwrap_or_else(|e| e.into_inner());
                }
            };
            wait_us += started.elapsed().as_micros() as u64;
            in_flight -= finished.len();
            if stopped {
                continue;
            }
            // What waited for them is walked again (and what waited for
            // that, once planned).
            let started = std::time::Instant::now();
            let mut again: Vec<Walked> = Vec::new();
            with(|prefetch| {
                for (key, outcome) in finished {
                    let usable = matches!(outcome, Outcome::Analyzed { .. });
                    prefetch.results.insert(key.clone(), outcome);
                    if !usable {
                        continue;
                    }
                    prefetch.done(Waiter::Application(key), &mut again);
                }
            });
            while let Some(walked) = again.pop() {
                let aspects = with(|prefetch| {
                    if !prefetch.pending.remove(&walked) {
                        return None;
                    }
                    prefetch.arguments.get(&walked).cloned()
                })
                .flatten();
                let Some(aspects) = aspects else {
                    continue;
                };
                stat(2, 1);
                match self.dry_walk(state, configured, &walked.0, walked.1, aspects) {
                    Ok(()) => {
                        with(|prefetch| prefetch.done(Waiter::Dependency(walked), &mut again));
                    }
                    Err(error) if error == PENDING => {}
                    Err(_) => {
                        stopped = true;
                        break;
                    }
                }
            }
            dry_us += started.elapsed().as_micros() as u64;
        }
        stat(6, dry_us);
        stat(7, wait_us);
        with(|prefetch| {
            prefetch.mode = Mode::Replay;
            prefetch.planned.clear();
            prefetch.pending.clear();
            prefetch.waiting.clear();
            prefetch.outstanding.clear();
            prefetch.arguments.clear();
            prefetch.jobs.clear();
            prefetch.dry.clear();
        });
    }

    /// The dry walk over `rule`'s aspect-applied edges (the aspect part of
    /// its prepare's dependency views): [`PENDING`] while an application
    /// waits for its analysis.
    fn dry_aspect_edges(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Rule,
    ) -> Result<(), String> {
        let edges = self.rule_edges(rule, key.config);
        let exec_config = self
            .exec_configurations
            .get(key)
            .copied()
            .unwrap_or(key.config);
        let dependencies = Self::custom_dependencies_in(self, rule, key, &edges, exec_config)
            .map_err(|_| STOP.to_owned())?;
        let mut pending = false;
        for (label, edge_config, _, attribute) in &dependencies.labels {
            let Some(aspects) = dependencies.edge_aspects.get(attribute) else {
                continue;
            };
            if aspects.is_empty() {
                continue;
            }
            match self.dry_walk(state, configured, label, *edge_config, aspects.clone()) {
                Ok(()) => {}
                Err(error) if error == PENDING => pending = true,
                Err(_) => return Err(STOP.to_owned()),
            }
        }
        if pending {
            Err(PENDING.to_owned())
        } else {
            Ok(())
        }
    }

    /// One aspect-applied dependency walked dry.
    fn dry_walk(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        label: &Label,
        edge_config: ConfigId,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
    ) -> Result<(), String> {
        let ActionPlanning {
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            producers,
            nodes,
            source_inputs,
            client_environment,
            status_client_environment,
            native_output_groups,
            validation,
            target_identities,
            aspect_providers,
            aspect_target_identities,
            workspace_status,
            views,
            input_groups,
            input_memos,
            listed_tree_actions,
            ..
        } = state;
        let mut planning = PlanState {
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            native_output_groups,
            validation,
            target_identities,
            aspect_providers,
            aspect_target_identities,
            nodes,
            producers,
            source_inputs,
            client_environment,
            status_client_environment,
            workspace_status,
            views,
            input_groups: &*input_groups,
            input_memos: &*input_memos,
            listed_tree_actions,
        };
        self.plan_aspect_dependency(&mut planning, configured, label, edge_config, aspects)
            .map(|_| ())
    }
}

/// The prefetch of the turn's entry is over (its prepare ran).
pub(super) fn end_prefetch() {
    PREFETCH.with(|cell| cell.borrow_mut().take());
}

/// One application's analysis, isolated like a worker's compute: anything
/// observable (effects, a sentinel, an error, a panic) leaves it to the
/// prepare.
fn analyze(job: Job) -> Outcome {
    let Job {
        key,
        application,
        output_root,
        action_owner,
        action_env,
        runfiles,
        loaded_functions,
        run_under,
    } = job;
    let analyzed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::debug_panic::at("t3-aspect");
        let isolation = Isolation::enter();
        let outputs = crate::analysis::ConfiguredOutputs {
            output_root: &output_root,
            action_owner: &action_owner,
            action_env,
            runfiles,
            loaded_functions: loaded_functions.as_ref(),
            run_under,
        };
        let started = std::time::Instant::now();
        let (analyzed, printed) = crate::analysis::analyze_aspect_buffered(&application, &outputs);
        note_cost(&key.aspect, started.elapsed().as_micros() as u64);
        let (effects, sentinel) = isolation.finish();
        let quiet = effects.console.is_empty()
            && effects.observations.is_empty()
            && effects.roots.is_empty();
        match analyzed {
            Ok(analyzed) if quiet && !sentinel => Outcome::Analyzed {
                analyzed: Box::new(analyzed),
                printed,
            },
            _ => Outcome::Unusable,
        }
    }));
    // A panic here is the prepare's again, raised in order (its analysis
    // redone at its point).
    analyzed.unwrap_or(Outcome::Unusable)
}
