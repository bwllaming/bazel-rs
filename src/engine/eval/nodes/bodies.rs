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
use super::super::key::{NodeId, NodeKey};
use super::super::scheduler::Bodies;
use super::super::shared::EvalShared;
use super::super::transcript::Event;
use super::super::value::{Ct1Value, NodeValue};
use super::eval_host::FirstVisit;

/// The S1 coordinator state the bodies share (one thread in S1).
pub(crate) struct Coord<'e> {
    /// The engine's loading services (package loading, target lookups,
    /// package groups, aspect definitions, registration patterns):
    /// `Pkg`/`Tgt`/`Group` bodies over [`EvalShared`] in S2.
    pub(crate) engine: &'e mut Engine,
    pub(crate) first: FirstVisit,
    /// Record `Access`/`Source` events (incremental tracking is on).
    pub(crate) tracking: bool,
    /// A `Flush` was emitted (a config_setting with Starlark flag_values
    /// keys: its CT2 dependencies are S2).
    pub(crate) saw_flush: bool,
    /// The attempts running, outermost first.
    stack: Vec<NodeKey>,
    /// The done CT1 value of each ConfiguredTargetKey's first variant.
    ct1_done: FxHashMap<ConfiguredTargetKey, Arc<Ct1Value>>,
    /// The source files whose analysis succeeded (`analyzed_sources`).
    sources: FxHashSet<Label>,
}

impl Coord<'_> {
    pub(crate) fn ct1(&self, key: &ConfiguredTargetKey) -> Option<Arc<Ct1Value>> {
        self.ct1_done.get(key).cloned()
    }

    pub(crate) fn source_analyzed(&self, label: &Label) -> bool {
        self.sources.contains(label)
    }

    pub(crate) fn in_progress(&self, key: &NodeKey) -> bool {
        self.stack.contains(key)
    }

    /// The configured targets being analyzed, outermost first, since the
    /// last detached analysis (a platform's tree, a registration: the
    /// walk's fresh `active`).
    pub(crate) fn active_ct1(&self) -> Vec<ConfiguredTargetKey> {
        let mut active = Vec::new();
        for key in self.stack.iter().rev() {
            match key {
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
        self.cx.need(keys)
    }

    fn emit(&mut self, event: Event) {
        if event == Event::Flush {
            self.coord().saw_flush = true;
        }
        self.cx.emit(event);
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
                first: FirstVisit::default(),
                tracking,
                saw_flush: false,
                stack: Vec::new(),
                ct1_done: FxHashMap::default(),
                sources: FxHashSet::default(),
            }),
        }
    }

    pub(crate) fn coord(&self) -> MutexGuard<'_, Coord<'e>> {
        self.coord
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn pop(&self) {
        self.coord().stack.pop();
    }
}

impl Bodies for Phase1Bodies<'_, '_> {
    type Attempt = ();

    fn begin(&self, _id: NodeId, key: &NodeKey) {
        self.coord().stack.push(key.clone());
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
        let key = coord.stack.pop();
        match (key, &value) {
            (Some(NodeKey::Ct1(ct)), NodeValue::Ct1(ct1)) => {
                if coord.first.canonical.get(&ct.key) == Some(&ct) {
                    coord.ct1_done.insert(ct.key.clone(), Arc::clone(ct1));
                }
            }
            (Some(NodeKey::Src(label)), NodeValue::Src(Ok(()))) => {
                coord.sources.insert(label);
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
