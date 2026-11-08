//! The scheduler over synthetic DAGs: toy bodies whose value is
//! `1 + Σ dependency values` (needed in batches, one restart per batch),
//! errors propagated as values, repository demands, cycles answered by
//! [`DfsCycles`], at 1 (inline), 2, 8, 32 and 64 threads, with jitter.

use std::sync::Mutex;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use super::*;
use crate::engine::eval::cycles::DfsCycles;
use crate::label::Label;

fn tgt(i: usize) -> NodeKey {
    NodeKey::Tgt(Label {
        repository: String::new(),
        package: "g".into(),
        name: i.to_string(),
    })
}

fn memo(name: &str) -> NodeKey {
    NodeKey::FlagScope(Label {
        repository: String::new(),
        package: "m".into(),
        name: name.into(),
    })
}

fn number(value: u64) -> NodeValue {
    NodeValue::ModuleExecPlatforms(Ok(Arc::new(vec![(value.to_string(), String::new())])))
}

fn failure(text: String) -> NodeValue {
    NodeValue::ModuleExecPlatforms(Err(text))
}

/// The number of a value (`Err`: its error text).
fn read(value: &NodeValue) -> Result<u64, String> {
    match value {
        NodeValue::ModuleExecPlatforms(Ok(pairs)) => Ok(pairs[0].0.parse().unwrap()),
        NodeValue::ModuleExecPlatforms(Err(text)) => Err(text.clone()),
        _ => panic!("not a test value"),
    }
}

/// One toy node.
#[derive(Clone, Default)]
struct Spec {
    /// Dependency batches, needed one after another.
    batches: Vec<Vec<NodeKey>>,
    /// The node fails (an error value).
    fails: bool,
    /// Repositories it needs, one per round, before its batches.
    repos: Vec<String>,
    /// A `BackEdge` becomes a cycle error value (the walk's reentrancy
    /// guards); otherwise it is returned.
    breaks_cycles: bool,
    /// Sleep in the body.
    sleep: Duration,
}

#[derive(Default)]
struct Toy {
    specs: FxHashMap<NodeKey, Spec>,
    /// Repositories the lane published.
    published: Arc<Mutex<FxHashSet<String>>>,
    /// Single-flight check: nodes running now.
    running: Mutex<FxHashSet<NodeKey>>,
    /// Attempts per key.
    runs: Mutex<FxHashMap<NodeKey, u32>>,
    /// Keys in attempt-start order.
    order: Mutex<Vec<NodeKey>>,
    /// Threads coordinator-only nodes ran on.
    coordinator_threads: Mutex<Vec<ThreadId>>,
    /// begin, finish, abandon calls.
    hooks: [AtomicU64; 3],
    /// (start of each attempt, end of the successful one) per key.
    times: Option<Mutex<FxHashMap<NodeKey, (Instant, Instant)>>>,
}

impl Toy {
    fn spec(&mut self, key: NodeKey) -> &mut Spec {
        self.specs.entry(key).or_default()
    }

    /// Forget the bookkeeping of earlier evaluations (repositories too).
    fn reset(&self) {
        self.runs.lock().unwrap().clear();
        self.order.lock().unwrap().clear();
        self.coordinator_threads.lock().unwrap().clear();
        for hook in &self.hooks {
            hook.store(0, Ordering::Relaxed);
        }
        self.published.lock().unwrap().clear();
    }

    fn runs(&self, key: &NodeKey) -> u32 {
        self.runs.lock().unwrap().get(key).copied().unwrap_or(0)
    }

    /// The expected value of `key` (sequential recursion; `None` on a
    /// cycle).
    fn expected(
        &self,
        key: &NodeKey,
        memo: &mut FxHashMap<NodeKey, Result<u64, String>>,
    ) -> Result<u64, String> {
        if let Some(value) = memo.get(key) {
            return value.clone();
        }
        let spec = self.specs.get(key).cloned().unwrap_or_default();
        let mut sum = 1u64;
        let mut error = None;
        for batch in &spec.batches {
            for dep in batch {
                match self.expected(dep, memo) {
                    Ok(value) => sum = sum.wrapping_add(value),
                    Err(text) => error = error.or(Some(text)),
                }
            }
        }
        let value = if spec.fails {
            Err(format!("{key:?} failed"))
        } else if let Some(text) = error {
            Err(text)
        } else {
            Ok(sum)
        };
        memo.insert(key.clone(), value.clone());
        value
    }
}

impl Bodies for Toy {
    type Attempt = NodeKey;

    fn begin(&self, _id: NodeId, key: &NodeKey) -> NodeKey {
        self.hooks[0].fetch_add(1, Ordering::Relaxed);
        key.clone()
    }

    fn finish(&self, attempt: NodeKey, value: NodeValue) -> NodeValue {
        self.hooks[1].fetch_add(1, Ordering::Relaxed);
        if let Some(times) = &self.times {
            times.lock().unwrap().get_mut(&attempt).unwrap().1 = Instant::now();
        }
        value
    }

    fn abandon(&self, _attempt: NodeKey) {
        self.hooks[2].fetch_add(1, Ordering::Relaxed);
    }

    fn run(&self, cx: &mut dyn Cx, key: &NodeKey) -> Result<NodeValue, Missing> {
        if let Some(times) = &self.times {
            let now = Instant::now();
            times.lock().unwrap().insert(key.clone(), (now, now));
        }
        assert!(
            self.running.lock().unwrap().insert(key.clone()),
            "{key:?} runs twice at once"
        );
        *self.runs.lock().unwrap().entry(key.clone()).or_default() += 1;
        self.order.lock().unwrap().push(key.clone());
        if key.coordinator_only() {
            self.coordinator_threads
                .lock()
                .unwrap()
                .push(std::thread::current().id());
        }
        let result = self.body(cx, key);
        self.running.lock().unwrap().remove(key);
        result
    }

    fn target_platform(&self, config: ConfigId) -> NodeKey {
        let _ = config;
        memo("target_platform")
    }

    fn stops_build(&self, _key: &NodeKey, value: &NodeValue) -> bool {
        value.error().is_some()
    }
}

impl Toy {
    fn body(&self, cx: &mut dyn Cx, key: &NodeKey) -> Result<NodeValue, Missing> {
        let spec = self.specs.get(key).cloned().unwrap_or_default();
        if !spec.sleep.is_zero() {
            std::thread::sleep(spec.sleep);
        }
        for repo in &spec.repos {
            if !self.published.lock().unwrap().contains(repo) {
                return Err(Missing::Repo(repo.clone()));
            }
        }
        let mut sum = 1u64;
        let mut error = None;
        for batch in &spec.batches {
            let values = match cx.need(batch) {
                Ok(values) => values,
                Err(Missing::BackEdge(child)) if spec.breaks_cycles => {
                    cx.emit(Event::Error("cycle".into()));
                    return Ok(failure(format!("cycle through {child:?}")));
                }
                Err(missing) => return Err(missing),
            };
            assert_eq!(values.len(), batch.len());
            for value in values.iter() {
                match read(value) {
                    Ok(value) => sum = sum.wrapping_add(value),
                    Err(text) => error = error.or(Some(text)),
                }
            }
        }
        cx.emit(Event::Warn(format!("{}", cx.current().0)));
        Ok(if spec.fails {
            failure(format!("{key:?} failed"))
        } else if let Some(text) = error {
            failure(text)
        } else {
            number(sum)
        })
    }
}

/// A repository lane double: publishes (or not) on the calling thread.
struct Lane {
    published: Arc<Mutex<FxHashSet<String>>>,
    order: Vec<String>,
    threads: Vec<ThreadId>,
    publish: bool,
}

impl Lane {
    fn of(toy: &Toy) -> Self {
        Self {
            published: Arc::clone(&toy.published),
            order: Vec::new(),
            threads: Vec::new(),
            publish: true,
        }
    }
}

impl RepoLane for Lane {
    fn fetch(&mut self, name: &str) {
        assert_no_locks("test fetch");
        self.order.push(name.to_owned());
        self.threads.push(std::thread::current().id());
        std::thread::sleep(Duration::from_micros(200));
        if self.publish {
            self.published.lock().unwrap().insert(name.to_owned());
        }
    }
}

fn config(threads: usize) -> EvalConfig {
    EvalConfig {
        threads,
        keep_going: true,
        jitter_micros: 0,
        ..EvalConfig::default()
    }
}

/// Run `f` on a thread with a big stack (the inline driver and the
/// expectation recurse as deep as the graph).
fn big<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(1 << 30)
            .spawn_scoped(scope, f)
            .unwrap()
            .join()
            .unwrap()
    })
}

const MATRIX: [usize; 5] = [1, 2, 8, 32, 64];

/// Evaluate `roots`, then check every node's value against the
/// sequential expectation and the restart bound.
fn check(toy: &Toy, roots: &[NodeKey], config: EvalConfig) -> Outcome {
    toy.reset();
    let evaluator = Evaluator::new(toy, config.clone());
    let mut lane = Lane::of(toy);
    let outcome = evaluator.evaluate(roots, &mut lane, &mut DfsCycles::default());
    assert!(outcome.complete, "{config:?}: {outcome:?}");
    let ids: Vec<NodeId> = evaluator.graph().ids().collect();
    let expected: Vec<Result<u64, String>> = big(|| {
        let mut memo = FxHashMap::default();
        ids.iter()
            .map(|&id| toy.expected(evaluator.graph().key(id), &mut memo))
            .collect()
    });
    for (&id, expected) in ids.iter().zip(expected) {
        let key = evaluator.graph().key(id);
        let value = evaluator.graph().value(id).expect("every node is done");
        assert_eq!(read(value), expected, "{key:?} at {config:?}");
        let spec = toy.specs.get(key).cloned().unwrap_or_default();
        let bound = (spec.batches.len() + spec.repos.len() + 1) as u32;
        assert!(
            toy.runs(key) <= bound,
            "{key:?} ran {} times",
            toy.runs(key)
        );
        assert!(evaluator.graph().entry(id).transcript().is_some());
    }
    let stats = evaluator.stats();
    assert_eq!(stats.done as usize, evaluator.graph().len());
    let [begun, finished, abandoned] = toy.hooks.each_ref().map(|h| h.load(Ordering::Relaxed));
    assert_eq!(begun, finished + abandoned);
    assert_eq!(finished, stats.done);
    assert_eq!(begun, stats.runs);
    outcome
}

/// A seeded random DAG over `n` nodes (edges to higher indices only),
/// with batches, a few shared memo nodes and occasional repositories.
fn random_dag(n: usize, seed: u64) -> Toy {
    let mut toy = Toy::default();
    let mut state = seed;
    let mut next = move || {
        state = crate::engine::loading_jitter_roll(state, 7);
        state
    };
    for i in 0..n {
        let spec = toy.spec(tgt(i));
        let batches = (next() % 4) as usize;
        for _ in 0..batches {
            let width = (next() % 4) as usize;
            let batch: Vec<NodeKey> = (0..width)
                .filter_map(|_| {
                    let span = (n - i - 1) as u64;
                    (span > 0).then(|| tgt(i + 1 + (next() % span.min(50)) as usize))
                })
                .collect();
            if !batch.is_empty() {
                spec.batches.push(batch);
            }
        }
        if next() % 10 == 0 {
            spec.batches.push(vec![memo(&format!("m{}", next() % 5))]);
        }
        if next() % 25 == 0 {
            spec.repos.push(format!("repo{}", next() % 4));
        }
    }
    toy
}

#[test]
fn a_diamond_at_every_thread_count() {
    for threads in MATRIX {
        let mut toy = Toy::default();
        toy.spec(tgt(0)).batches = vec![vec![tgt(1), tgt(2)]];
        toy.spec(tgt(1)).batches = vec![vec![tgt(3)]];
        toy.spec(tgt(2)).batches = vec![vec![tgt(3)]];
        check(&toy, &[tgt(0)], config(threads));
        assert_eq!(toy.runs(&tgt(3)), 1);
        assert_eq!(toy.runs(&tgt(0)), if threads == 1 { 1 } else { 2 });
    }
}

#[test]
fn the_inline_driver_is_depth_first() {
    let mut toy = Toy::default();
    toy.spec(tgt(0)).batches = vec![vec![tgt(1), tgt(2)], vec![tgt(3)]];
    toy.spec(tgt(1)).batches = vec![vec![tgt(4)]];
    toy.spec(tgt(2)).batches = vec![vec![tgt(4), tgt(5)]];
    check(&toy, &[tgt(0)], config(1));
    let order = toy.order.lock().unwrap().clone();
    assert_eq!(order, [0, 1, 4, 2, 5, 3].map(tgt));
}

#[test]
fn a_deep_chain() {
    const N: usize = 10_000;
    let mut toy = Toy::default();
    for i in 0..N - 1 {
        toy.spec(tgt(i)).batches = vec![vec![tgt(i + 1)]];
    }
    // The inline driver recurses (the walk's DFS): a big stack, as the
    // analysis runs on.
    big(|| {
        for threads in [1, 2, 8] {
            let outcome = check(&toy, &[tgt(0)], config(threads));
            assert!(outcome.unresolved.is_empty());
        }
    });
}

#[test]
fn a_wide_fan_out() {
    const N: usize = 100_000;
    let mut toy = Toy::default();
    toy.spec(tgt(0)).batches = vec![(1..=N).map(tgt).collect()];
    for threads in [1, 8, 64] {
        check(&toy, &[tgt(0)], config(threads));
    }
}

#[test]
fn random_dags_with_jitter_at_every_thread_count() {
    for (seed, threads) in [(1, 1), (2, 2), (3, 8), (4, 32), (5, 64), (6, 8)] {
        let toy = random_dag(1500, seed);
        let roots: Vec<NodeKey> = (0..20).map(tgt).collect();
        let config = EvalConfig {
            jitter_micros: 40,
            ..config(threads)
        };
        big(|| check(&toy, &roots, config));
    }
}

#[test]
fn random_dags_without_jitter_stress() {
    for round in 0..4u64 {
        for threads in [2, 32, 64] {
            let toy = random_dag(4000, 100 + round);
            let roots: Vec<NodeKey> = (0..50).map(tgt).collect();
            big(|| check(&toy, &roots, config(threads)));
        }
    }
}

#[test]
fn several_missing_rounds_restart_the_node_once_each() {
    for threads in MATRIX {
        let mut toy = Toy::default();
        toy.spec(tgt(0)).batches = (1..=4).map(|i| vec![tgt(i), tgt(i + 10)]).collect();
        toy.spec(tgt(0)).repos = vec!["a".into()];
        for i in 1..=4 {
            toy.spec(tgt(i)).sleep = Duration::from_micros(300);
        }
        check(&toy, &[tgt(0)], config(threads));
        let expected = if threads == 1 { 2 } else { 6 };
        assert_eq!(toy.runs(&tgt(0)), expected, "threads {threads}");
    }
}

#[test]
fn single_flight_under_contention() {
    let mut toy = Toy::default();
    toy.spec(memo("shared")).sleep = Duration::from_millis(5);
    toy.spec(tgt(0)).batches = vec![(1..=1000).map(tgt).collect()];
    for i in 1..=1000 {
        toy.spec(tgt(i)).batches = vec![vec![memo("shared")]];
    }
    check(&toy, &[tgt(0)], config(64));
    assert_eq!(toy.runs(&memo("shared")), 1);
}

#[test]
fn repositories_are_fetched_once_on_the_coordinator_in_demand_order() {
    for threads in MATRIX {
        let mut toy = Toy::default();
        toy.spec(tgt(0)).batches = vec![(1..=200).map(tgt).collect()];
        for i in 1..=200 {
            toy.spec(tgt(i)).repos = vec![format!("r{}", i % 7), format!("s{}", i % 3)];
        }
        let evaluator = Evaluator::new(&toy, config(threads));
        let mut lane = Lane::of(&toy);
        let outcome = evaluator.evaluate(&[tgt(0)], &mut lane, &mut DfsCycles::default());
        assert!(outcome.complete);
        assert_eq!(lane.order.len(), 10, "{:?}", lane.order);
        let distinct: FxHashSet<_> = lane.order.iter().collect();
        assert_eq!(distinct.len(), 10);
        let me = std::thread::current().id();
        assert!(lane.threads.iter().all(|&thread| thread == me));
        assert_eq!(evaluator.stats().fetches, 10);
        for i in 1..=200 {
            assert!(toy.runs(&tgt(i)) <= 3);
        }
    }
}

#[test]
fn a_repository_the_lane_cannot_publish_leaves_the_node_unresolved() {
    for threads in [1, 8] {
        let mut toy = Toy::default();
        toy.spec(tgt(0)).batches = vec![vec![tgt(1)]];
        toy.spec(tgt(1)).repos = vec!["broken".into()];
        let evaluator = Evaluator::new(&toy, config(threads));
        let mut lane = Lane::of(&toy);
        lane.publish = false;
        let outcome = evaluator.evaluate(&[tgt(0)], &mut lane, &mut DfsCycles::default());
        assert!(!outcome.complete);
        assert_eq!(lane.order, ["broken"]);
        let id = evaluator.graph().get(&tgt(1)).unwrap();
        assert!(outcome.unresolved.contains(&id));
        assert_eq!(
            evaluator.graph().entry(id).inner.lock().wait,
            Wait::Unresolvable("broken".into())
        );
    }
}

#[test]
fn coordinator_only_nodes_run_on_the_coordinator() {
    for threads in MATRIX {
        let mut toy = Toy::default();
        toy.spec(tgt(0)).batches = vec![(1..=50).map(tgt).collect()];
        for i in 1..=50 {
            toy.spec(tgt(i)).batches =
                vec![vec![NodeKey::Status], vec![NodeKey::ModuleExecPlatforms]];
        }
        toy.spec(NodeKey::ModuleExecPlatforms).batches = vec![vec![tgt(99)]];
        check(&toy, &[tgt(0)], config(threads));
        let me = std::thread::current().id();
        let threads_seen = toy.coordinator_threads.lock().unwrap().clone();
        assert!(!threads_seen.is_empty());
        assert!(threads_seen.iter().all(|&thread| thread == me));
        assert_eq!(toy.runs(&NodeKey::Status), 1);
    }
}

#[test]
fn a_target_platform_is_an_ordinary_dependency() {
    struct Platform(Toy);
    impl Bodies for Platform {
        type Attempt = ();
        fn begin(&self, _id: NodeId, _key: &NodeKey) {}
        fn run(&self, cx: &mut dyn Cx, key: &NodeKey) -> Result<NodeValue, Missing> {
            if *key == tgt(0) {
                let platform = memo("target_platform");
                if !self.0.published.lock().unwrap().contains("platform") {
                    self.0.published.lock().unwrap().insert("platform".into());
                    return Err(Missing::TargetPlatform(
                        crate::configuration::Configuration::default().id,
                    ));
                }
                let value = cx.need_one(platform)?;
                return Ok(number(read(&value).unwrap() + 1));
            }
            self.0.run(cx, key)
        }
        fn target_platform(&self, config: ConfigId) -> NodeKey {
            self.0.target_platform(config)
        }
    }
    for threads in [1, 8] {
        let toy = Platform(Toy::default());
        let evaluator = Evaluator::new(&toy, config(threads));
        let outcome = evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut DfsCycles::default());
        assert!(outcome.complete);
        assert_eq!(read(evaluator.value(&tgt(0)).unwrap()), Ok(2));
        assert_eq!(toy.0.runs(&memo("target_platform")), 1);
    }
}

#[test]
fn errors_are_values_with_keep_going() {
    for threads in MATRIX {
        let mut toy = Toy::default();
        toy.spec(tgt(0)).batches = vec![vec![tgt(1), tgt(2)]];
        toy.spec(tgt(1)).fails = true;
        for i in 2..30 {
            toy.spec(tgt(i)).batches = vec![vec![tgt(i + 1)]];
        }
        let outcome = check(&toy, &[tgt(0)], config(threads));
        assert!(outcome.first_error.is_none());
        assert!(outcome.deferred.is_empty());
    }
}

#[test]
fn the_first_error_without_keep_going_turns_frontier_only() {
    for threads in [2, 8, 32] {
        let mut toy = Toy::default();
        toy.spec(tgt(0)).batches = vec![vec![tgt(1), tgt(2)]];
        toy.spec(tgt(1)).fails = true;
        for i in 2..40 {
            toy.spec(tgt(i)).batches = vec![vec![tgt(i + 1)]];
            toy.spec(tgt(i)).sleep = Duration::from_millis(1);
        }
        let config = EvalConfig {
            keep_going: false,
            ..config(threads)
        };
        let evaluator = Evaluator::new(&toy, config);
        let mut cycles = DfsCycles::default();
        let outcome = evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut cycles);
        assert!(!outcome.complete, "{outcome:?}");
        assert!(evaluator.frontier_only());
        assert_eq!(outcome.first_error, evaluator.graph().get(&tgt(1)));
        assert_eq!(outcome.deferred.len(), 1, "{outcome:?}");
        assert!(toy.runs(&tgt(39)) == 0, "the chain's end never ran");
        // The linearizer wants the root: it and everything it waits for
        // run again.
        evaluator.boost(&tgt(0), 0);
        let outcome = evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut cycles);
        assert!(outcome.complete, "{outcome:?}");
        assert!(read(evaluator.value(&tgt(0)).unwrap()).is_err());
        assert_eq!(read(evaluator.value(&tgt(2)).unwrap()), Ok(39));
    }
}

#[test]
fn boosted_nodes_run_first() {
    let mut toy = Toy::default();
    let roots: Vec<NodeKey> = (0..400).map(tgt).collect();
    for i in 0..400 {
        toy.spec(tgt(i)).sleep = Duration::from_micros(50);
    }
    let evaluator = Evaluator::new(&toy, config(2));
    let boosted: Vec<NodeKey> = (390..400).rev().map(tgt).collect();
    for (priority, key) in boosted.iter().enumerate() {
        evaluator.boost(key, priority as u64);
    }
    let outcome = evaluator.evaluate(&roots, &mut NoRepositories, &mut DfsCycles::default());
    assert!(outcome.complete);
    let order = toy.order.lock().unwrap().clone();
    let early: FxHashSet<&NodeKey> = order[..boosted.len() + 2].iter().collect();
    for key in &boosted {
        assert!(early.contains(key), "{key:?} ran late: {:?}", &order[..14]);
    }
}

/// Toy cycles: `edges` of (node, its one batch).
fn cyclic(edges: &[(usize, &[usize])], breakers: &[usize]) -> Toy {
    let mut toy = Toy::default();
    for (node, deps) in edges {
        toy.spec(tgt(*node)).batches = vec![deps.iter().map(|&d| tgt(d)).collect()];
    }
    for breaker in breakers {
        toy.spec(tgt(*breaker)).breaks_cycles = true;
    }
    toy
}

#[test]
fn cycles_are_answered_with_back_edges_like_the_dfs() {
    for threads in MATRIX {
        // 0 → 1 → 2 → 1: 2's need of 1 is the back edge (the DFS's).
        let toy = cyclic(&[(0, &[1]), (1, &[2]), (2, &[1, 3])], &[2]);
        let evaluator = Evaluator::new(&toy, config(threads));
        let mut cycles = DfsCycles::default();
        let outcome = evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut cycles);
        assert!(outcome.complete, "{threads}: {outcome:?}");
        let one = evaluator.graph().get(&tgt(1)).unwrap();
        assert_eq!(
            read(evaluator.value(&tgt(2)).unwrap()),
            Err(format!("cycle through {one:?}"))
        );
        assert!(read(evaluator.value(&tgt(0)).unwrap()).is_err());
        // Inline: the active chain answers at once; pool: one quiescence.
        assert_eq!(cycles.answers, usize::from(threads > 1));
        assert_eq!(evaluator.stats().back_edges, 1);
    }
}

#[test]
fn self_cycles_and_cycles_through_memo_nodes() {
    for threads in MATRIX {
        let mut toy = cyclic(&[(0, &[1, 2]), (1, &[1])], &[1]);
        toy.spec(tgt(2)).batches = vec![vec![memo("platform")]];
        toy.spec(memo("platform")).batches = vec![vec![tgt(3)]];
        toy.spec(tgt(3)).batches = vec![vec![memo("platform")]];
        toy.spec(memo("platform")).breaks_cycles = false;
        toy.spec(tgt(3)).breaks_cycles = true;
        let evaluator = Evaluator::new(&toy, config(threads));
        let outcome = evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut DfsCycles::default());
        assert!(outcome.complete, "{threads}: {outcome:?}");
        assert!(
            read(evaluator.value(&tgt(1)).unwrap())
                .unwrap_err()
                .contains("cycle")
        );
        assert!(
            read(evaluator.value(&tgt(3)).unwrap())
                .unwrap_err()
                .contains("cycle")
        );
        assert!(read(evaluator.value(&memo("platform")).unwrap()).is_err());
    }
}

#[test]
fn an_unbroken_cycle_stops_with_its_nodes_unresolved() {
    for threads in MATRIX {
        let toy = cyclic(&[(0, &[1, 4]), (1, &[2]), (2, &[3]), (3, &[1])], &[]);
        let evaluator = Evaluator::new(&toy, config(threads));
        let mut cycles = DfsCycles::default();
        let outcome = evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut cycles);
        assert!(!outcome.complete);
        let ids: Vec<NodeId> = [0, 1, 2, 3]
            .map(|i| evaluator.graph().get(&tgt(i)).unwrap())
            .to_vec();
        for id in &ids {
            assert!(outcome.unresolved.contains(id), "{threads}: {outcome:?}");
        }
        // 4 is outside the cycle and done.
        assert!(evaluator.value(&tgt(4)).is_some());
        assert!(cycles.answers <= 1);
    }
}

#[test]
fn transcripts_keep_the_successful_attempt_only() {
    for threads in [1, 8] {
        let mut toy = Toy::default();
        toy.spec(tgt(0)).batches = vec![vec![tgt(1)], vec![tgt(2)]];
        let evaluator = Evaluator::new(&toy, config(threads));
        evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut DfsCycles::default());
        let id = evaluator.graph().get(&tgt(0)).unwrap();
        let transcript = evaluator.graph().entry(id).transcript().unwrap();
        assert_eq!(transcript.events, [Event::Warn(id.0.to_string())]);
    }
}

#[test]
fn worker_threads_follow_cores_options_and_memory() {
    assert_eq!(worker_threads(64, 200, None), 64);
    assert_eq!(worker_threads(64, 8, None), 8);
    assert_eq!(worker_threads(64, 200, Some(1024)), 16);
    assert_eq!(worker_threads(64, 200, Some(10)), 1);
    assert_eq!(worker_threads(0, 0, None), 1);
}

#[test]
fn the_pool_has_big_stacks() {
    let pool = build_pool(2, WORKER_STACK);
    let deep = pool.install(|| {
        fn recurse(depth: u32) -> u32 {
            let buffer = [depth as u8; 1024];
            if depth == 0 {
                u32::from(buffer[0])
            } else {
                recurse(depth - 1) + u32::from(std::hint::black_box(buffer)[1])
            }
        }
        recurse(3_000)
    });
    assert!(deep > 0);
}

/// `cargo test --release --lib eval_scheduler_bench -- --ignored --nocapture`
#[test]
#[ignore]
fn eval_scheduler_bench() {
    // Per-node overhead (claim, schedule, run a trivial body, publish,
    // count down): a tree 64 × 64 × 50 (208,001 nodes; every inner node
    // restarts once), and the worst case, one node needing 200,000.
    let mut tree = Toy::default();
    let mut next = 1;
    let mut level = vec![0];
    for width in [64, 64, 50] {
        let mut below = Vec::new();
        for &parent in &level {
            let children: Vec<usize> = (next..next + width).collect();
            next += width;
            tree.spec(tgt(parent)).batches = vec![children.iter().map(|&c| tgt(c)).collect()];
            below.extend(children);
        }
        level = below;
    }
    let mut fan = Toy::default();
    fan.spec(tgt(0)).batches = vec![(1..=200_000).map(tgt).collect()];
    for (name, toy, nodes) in [("tree", &tree, next), ("fan-out", &fan, 200_001)] {
        for threads in [1, 2, 8, 32] {
            let bare = Bare(toy);
            let evaluator = Evaluator::new(&bare, config(threads));
            // Interning is the body's `need`, measured apart.
            for i in 0..nodes {
                evaluator.graph().intern(&tgt(i));
            }
            let start = Instant::now();
            let outcome =
                evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut DfsCycles::default());
            let elapsed = start.elapsed();
            assert!(outcome.complete);
            println!(
                "{name} {nodes} at {threads} threads: {elapsed:?} = {:.2} µs/node wall, {:.2} µs/node CPU-bound ({} attempts)",
                elapsed.as_secs_f64() * 1e6 / nodes as f64,
                elapsed.as_secs_f64() * 1e6 * threads as f64 / nodes as f64,
                evaluator.stats().runs,
            );
        }
    }
    // Chain latency: completion of a dependency → its dependent running.
    const CHAIN: usize = 5_000;
    for threads in [2, 8, 32] {
        let mut toy = Toy::default();
        for i in 0..CHAIN - 1 {
            toy.spec(tgt(i)).batches = vec![vec![tgt(i + 1)]];
        }
        toy.times = Some(Mutex::default());
        let evaluator = Evaluator::new(&toy, config(threads));
        let start = Instant::now();
        evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut DfsCycles::default());
        let elapsed = start.elapsed();
        let times = toy.times.as_ref().unwrap().lock().unwrap();
        let mut latencies: Vec<f64> = (0..CHAIN - 1)
            .map(|i| {
                let dependent = times[&tgt(i)].0;
                let dependency = times[&tgt(i + 1)].1;
                dependent
                    .saturating_duration_since(dependency)
                    .as_secs_f64()
                    * 1e6
            })
            .collect();
        latencies.sort_by(f64::total_cmp);
        println!(
            "chain {CHAIN} at {threads} threads: {elapsed:?}; completion → dependent running: median {:.2} µs, p99 {:.2} µs, max {:.2} µs",
            latencies[latencies.len() / 2],
            latencies[latencies.len() * 99 / 100],
            latencies[latencies.len() - 1],
        );
    }
    // Cross-thread wake: the dependent parked on another worker (two
    // chains that alternate: a waits on b, b on a', ...; each completion
    // wakes a node another job parked).
    let mut toy = Toy::default();
    for i in 0..CHAIN - 1 {
        toy.spec(tgt(i)).batches = vec![vec![tgt(i + 1), memo(&format!("{i}"))]];
        toy.spec(memo(&format!("{i}"))).sleep = Duration::from_micros(20);
    }
    toy.times = Some(Mutex::default());
    let evaluator = Evaluator::new(&toy, config(8));
    evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut DfsCycles::default());
    let times = toy.times.as_ref().unwrap().lock().unwrap();
    let mut latencies: Vec<f64> = (0..CHAIN - 1)
        .map(|i| {
            let dependent = times[&tgt(i)].0;
            let last = times[&tgt(i + 1)].1.max(times[&memo(&format!("{i}"))].1);
            dependent.saturating_duration_since(last).as_secs_f64() * 1e6
        })
        .collect();
    latencies.sort_by(f64::total_cmp);
    println!(
        "two-dependency chain {CHAIN} at 8 threads: last completion → dependent running: median {:.2} µs, p99 {:.2} µs, max {:.2} µs",
        latencies[latencies.len() / 2],
        latencies[latencies.len() * 99 / 100],
        latencies[latencies.len() - 1],
    );
}

/// The toy without its bookkeeping (the benchmark's bodies).
struct Bare<'t>(&'t Toy);

impl Bodies for Bare<'_> {
    type Attempt = ();
    fn begin(&self, _id: NodeId, _key: &NodeKey) {}
    fn run(&self, cx: &mut dyn Cx, key: &NodeKey) -> Result<NodeValue, Missing> {
        if let Some(spec) = self.0.specs.get(key) {
            for batch in &spec.batches {
                cx.need(batch)?;
            }
        }
        Ok(NodeValue::FlagScope(Ok(true)))
    }
    fn target_platform(&self, config: ConfigId) -> NodeKey {
        self.0.target_platform(config)
    }
}

#[test]
fn a_panicking_body_propagates_without_hanging() {
    struct Panics;
    impl Bodies for Panics {
        type Attempt = ();
        fn begin(&self, _id: NodeId, _key: &NodeKey) {}
        fn run(&self, cx: &mut dyn Cx, key: &NodeKey) -> Result<NodeValue, Missing> {
            if *key == tgt(0) {
                cx.need(&(1..=100).map(tgt).collect::<Vec<_>>())?;
            }
            if *key == tgt(50) {
                panic!("a body bug");
            }
            Ok(NodeValue::FlagScope(Ok(true)))
        }
        fn target_platform(&self, _config: ConfigId) -> NodeKey {
            memo("target_platform")
        }
    }
    for threads in [1, 8] {
        let result = std::panic::catch_unwind(|| {
            let evaluator = Evaluator::new(&Panics, config(threads));
            evaluator.evaluate(&[tgt(0)], &mut NoRepositories, &mut DfsCycles::default())
        });
        let message = result.unwrap_err();
        assert_eq!(message.downcast_ref::<&str>(), Some(&"a body bug"));
    }
}
