//! The phase-1 bodies as worker A's [`Bodies`]: run by A's evaluator
//! (S1: its inline driver, one thread, `need` evaluating in place = the
//! walk's DFS). [`Phase1Bodies`] holds the shared read view and the S1
//! coordinator ([`Coord`]: the engine's loading services and the
//! first-visit state pass 1 decides from S2 on); `begin`/`finish`/
//! `abandon` keep the stack of attempts (the walk's active chain for its
//! cycle texts) and the done CT1 values the bodies read without a need.
//!
//! Owner: worker B.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::configuration::ConfigId;
use crate::fast_hash::{FxHashMap, FxHashSet};
use crate::label::Label;

use super::super::super::{ConfiguredTargetKey, Engine};
use super::super::cx::{Cx, Missing, Vals};
use super::super::key::{CtKey, NodeId, NodeKey};
use super::super::linearize::driver::{Linearizer, Store};
use super::super::scheduler::Bodies;
use super::super::shared::EvalShared;
use super::super::transcript::Event;
use super::super::value::{Ct1Value, NodeValue};

/// The S1 coordinator state the bodies share (one thread in S1).
pub(crate) struct Coord<'e> {
    /// The engine's loading services (package loading, target lookups,
    /// package groups, aspect definitions, registration patterns):
    /// `Pkg`/`Tgt`/`Group` bodies over [`EvalShared`] in S2.
    pub(crate) engine: &'e mut Engine,
    /// Record `Access`/`Source` events (incremental tracking is on).
    pub(crate) tracking: bool,
    /// A `Flush` was emitted (a config_setting with Starlark flag_values
    /// keys: its CT2 dependencies are S2).
    pub(crate) saw_flush: bool,
    /// The attempts running, outermost first, each with what it emitted
    /// so far (worker C: the transcripts pass 1 replays, with what the
    /// engine's services did in between, `Coord::drain`).
    pub(crate) stack: Vec<Running>,
    /// The finished transcripts, the segments and the captured console
    /// writes (worker C).
    pub(crate) store: Store,
    /// `BAZEL_RS_ANALYSIS=eval`'s linearizer (pass 1 and the tape's
    /// application; `None` under BAZEL_RS_EVAL_VERIFY_PHASE1).
    pub(crate) lin: Option<Box<Linearizer>>,
    /// The CT1 value of each ConfiguredTargetKey's first variant done (in
    /// completion order: the bodies' reads of a key they did not reach,
    /// and whether a key was analyzed).
    pub(crate) ct1_done: FxHashMap<ConfiguredTargetKey, Arc<Ct1Value>>,
    /// Every done CT1 value, by variant (pass 1 decides which one is the
    /// configured target).
    pub(crate) ct1_values: FxHashMap<CtKey, Arc<Ct1Value>>,
    /// The source files whose analysis succeeded (`analyzed_sources`).
    pub(crate) sources: FxHashSet<Label>,
    /// BAZEL_RS_EVAL_RESTARTS: the needs answered `Missing` once.
    pub(crate) restarted: FxHashSet<(NodeId, Vec<NodeId>)>,
}

/// BAZEL_RS_EVAL_RESTARTS=<seed>: the restart test hook's seed.
fn restarts_seed() -> Option<u64> {
    static SEED: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *SEED.get_or_init(|| {
        // snapshot-unrecorded: a test hook; the analysis result must not change.
        std::env::var("BAZEL_RS_EVAL_RESTARTS")
            .ok()
            .and_then(|seed| seed.parse().ok())
    })
}

impl Coord<'_> {
    pub(crate) fn ct1(&self, key: &ConfiguredTargetKey) -> Option<Arc<Ct1Value>> {
        self.ct1_done.get(key).cloned()
    }

    pub(crate) fn source_analyzed(&self, label: &Label) -> bool {
        self.sources.contains(label)
    }

    /// The package groups being resolved (`resolve_group`'s active path).
    pub(crate) fn group_depth(&self) -> usize {
        self.stack
            .iter()
            .filter(|running| matches!(running.key, NodeKey::Group(_)))
            .count()
    }

    pub(crate) fn in_progress(&self, key: &NodeKey) -> bool {
        self.stack.iter().any(|running| running.key == *key)
    }

    /// The configured targets being analyzed, outermost first, since the
    /// last detached analysis (a platform's tree, a registration: the
    /// walk's fresh `active`).
    pub(crate) fn active_ct1(&self) -> Vec<ConfiguredTargetKey> {
        let mut active = Vec::new();
        for running in self.stack.iter().rev() {
            match &running.key {
                NodeKey::Ct1(ct) => active.push(ct.key.clone()),
                NodeKey::Platform(_) | NodeKey::RegToolchains(_) | NodeKey::RegExecPlatforms(_) => {
                    break;
                }
                _ => {}
            }
        }
        active.reverse();
        active
    }
}

/// One running attempt.
pub(crate) struct Running {
    pub(crate) id: NodeId,
    pub(crate) key: NodeKey,
    /// What it emitted so far (a rerun: starting with the abandoned
    /// attempt's events, which it re-emits; `Coord::push_event`).
    pub(crate) events: Vec<Event>,
    /// The leading events a rerun re-emits (the abandoned attempt's, up to
    /// its `Missing`): pass 1 may have replayed them already.
    pub(crate) replay: usize,
    /// How many of them the rerun re-emitted.
    pub(crate) cursor: usize,
    /// The events when the attempt first received a `Missing` (what it
    /// emitted while it stopped is not the rerun's).
    pub(crate) missing_at: Option<usize>,
}

/// What a phase-1 body needs of its context besides [`Cx`] in S1.
pub(crate) trait Phase1Cx<'e>: Cx {
    fn coord(&self) -> MutexGuard<'_, Coord<'e>>;
}

/// A body's context: A's `Cx` and the coordinator.
struct BodyCx<'a, 'e> {
    cx: &'a mut dyn Cx,
    coord: &'a Mutex<Coord<'e>>,
}

impl Cx for BodyCx<'_, '_> {
    fn need(&mut self, keys: &[NodeKey]) -> Result<Vals, Missing> {
        let vals = match self.cx.need(keys) {
            Ok(vals) => vals,
            Err(missing) => {
                self.coord().missing_here();
                return Err(missing);
            }
        };
        // BAZEL_RS_EVAL_RESTARTS=<seed> (a test hook): the first need of a
        // key by a node is answered `Missing` at random (one in four), as
        // a parallel evaluation parks an attempt: the attempt is abandoned
        // and the node runs again (its transcript must not change).
        if let Some(seed) = restarts_seed() {
            let current = self.cx.current();
            let ids: Vec<NodeId> = keys.iter().map(|key| self.cx.id_of(key)).collect();
            use std::hash::BuildHasher;
            let roll = crate::fast_hash::FxBuildHasher::default().hash_one((
                seed,
                current,
                format!("{keys:?}"),
            ));
            let fresh = self.coord().restarted.insert((current, ids.clone()));
            if fresh && roll % 4 == 0 {
                self.coord().missing_here();
                return Err(Missing::Nodes(ids));
            }
        }
        Ok(vals)
    }

    fn emit(&mut self, event: Event) {
        let linearized = {
            let mut coord = self.coord();
            if event == Event::Flush {
                coord.saw_flush = true;
            }
            coord.drain();
            coord.push_event(event.clone());
            coord.lin.is_some()
        };
        // Under BAZEL_RS_ANALYSIS=eval the coordinator's store is the
        // transcript of record (one copy: ~/code emits millions of events).
        if !linearized {
            self.cx.emit(event);
        }
    }

    fn current(&self) -> NodeId {
        self.cx.current()
    }

    fn id_of(&self, key: &NodeKey) -> NodeId {
        self.cx.id_of(key)
    }
}

impl<'e> Phase1Cx<'e> for BodyCx<'_, 'e> {
    fn coord(&self) -> MutexGuard<'_, Coord<'e>> {
        self.coord
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The phase-1 node bodies for A's evaluator.
pub(crate) struct Phase1Bodies<'s, 'e> {
    pub(crate) shared: &'s EvalShared,
    pub(crate) coord: Mutex<Coord<'e>>,
}

impl<'s, 'e> Phase1Bodies<'s, 'e> {
    pub(crate) fn new(shared: &'s EvalShared, engine: &'e mut Engine, tracking: bool) -> Self {
        Self {
            shared,
            coord: Mutex::new(Coord {
                engine,
                tracking,
                saw_flush: false,
                stack: Vec::new(),
                store: Store::default(),
                lin: None,
                ct1_done: FxHashMap::default(),
                ct1_values: FxHashMap::default(),
                sources: FxHashSet::default(),
                restarted: FxHashSet::default(),
            }),
        }
    }

    pub(crate) fn coord(&self) -> MutexGuard<'_, Coord<'e>> {
        self.coord
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn pop(&self) {
        let mut coord = self.coord();
        coord.drain();
        coord.abandon_attempt();
    }
}

impl Bodies for Phase1Bodies<'_, '_> {
    type Attempt = ();

    fn begin(&self, id: NodeId, key: &NodeKey) {
        let mut coord = self.coord();
        coord.drain();
        coord.begin_attempt(id, key.clone());
    }

    fn run(&self, cx: &mut dyn Cx, key: &NodeKey) -> Result<NodeValue, Missing> {
        let mut cx = BodyCx {
            cx,
            coord: &self.coord,
        };
        super::eval_host::body(self.shared, &mut cx, key)
    }

    fn finish(&self, _attempt: (), value: NodeValue) -> NodeValue {
        let mut coord = self.coord();
        coord.drain();
        let key = coord.finish_attempt(&value);
        match (key, &value) {
            (Some(NodeKey::Ct1(ct)), NodeValue::Ct1(ct1)) => {
                coord
                    .ct1_done
                    .entry(ct.key.clone())
                    .or_insert_with(|| Arc::clone(ct1));
                coord.ct1_values.insert(ct, Arc::clone(ct1));
            }
            (Some(NodeKey::Src(label)), NodeValue::Src(Ok(()))) => {
                coord.sources.insert(label);
            }
            (Some(NodeKey::Pkg(package)), NodeValue::Pkg(result)) => {
                coord.store.packages.insert(package, result.clone());
            }
            (Some(NodeKey::Group(label)), NodeValue::Group(Ok(group))) => {
                coord.store.groups.push((label, Arc::clone(group)));
            }
            _ => {}
        }
        value
    }

    fn abandon(&self, _attempt: ()) {
        self.pop();
    }

    fn target_platform(&self, config: ConfigId) -> NodeKey {
        let label = self
            .shared
            .configurations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(config)
            .native
            .target_platform()
            .clone();
        NodeKey::Platform(ConfiguredTargetKey::new(label, config))
    }
}
