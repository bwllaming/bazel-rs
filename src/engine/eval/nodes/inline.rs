//! B's inline driver (S1): [`Cx::need`] runs a missing node immediately,
//! depth first — the sequential walk's DFS — and answers an ancestor with
//! [`Missing::BackEdge`]. It stands in for worker A's inline driver for
//! the phase-1 bodies until that lands (the bodies only see
//! [`Phase1Cx`]); it also holds the S1 coordinator (the engine's loading
//! services) and the first-visit state pass 1 decides in S2.
//!
//! Owner: worker B.

use std::sync::Arc;

use crate::fast_hash::{FxHashMap, FxHashSet};

use super::super::super::{ConfiguredTargetKey, Engine};
use super::super::cx::{Cx, Missing, Vals};
use super::super::key::{Interner, NodeId, NodeKey};
use super::super::shared::EvalShared;
use super::super::transcript::{Event, Transcript};
use super::super::value::NodeValue;
use super::eval_host::{FirstVisit, Phase1Cx};

/// A node being computed.
struct Frame {
    id: NodeId,
    key: NodeKey,
    events: Vec<Event>,
}

/// The inline (one-thread, depth-first) driver of the phase-1 nodes.
pub(crate) struct Inline<'a> {
    engine: &'a mut Engine,
    shared: &'a EvalShared,
    pub(crate) interner: Interner,
    values: FxHashMap<NodeId, Arc<NodeValue>>,
    transcripts: FxHashMap<NodeId, Transcript>,
    stack: Vec<Frame>,
    on_stack: FxHashSet<NodeId>,
    pub(crate) first: FirstVisit,
    /// Events emitted outside any node (the requests' own analyses).
    pub(crate) root_events: Vec<Event>,
    /// Nodes computed.
    pub(crate) computed: usize,
    /// A `Flush` was emitted (a config_setting with Starlark flag_values
    /// keys: its CT2 dependencies are S2).
    pub(crate) saw_flush: bool,
    /// The last value of each failed node that is not memoized.
    failed: FxHashMap<NodeId, Arc<NodeValue>>,
    /// Record `Access`/`Source` events (incremental tracking).
    pub(crate) tracking: bool,
}

impl<'a> Inline<'a> {
    pub(crate) fn new(engine: &'a mut Engine, shared: &'a EvalShared) -> Self {
        Self {
            engine,
            shared,
            interner: Interner::default(),
            values: FxHashMap::default(),
            transcripts: FxHashMap::default(),
            stack: Vec::new(),
            on_stack: FxHashSet::default(),
            first: FirstVisit::default(),
            root_events: Vec::new(),
            computed: 0,
            saw_flush: false,
            failed: FxHashMap::default(),
            tracking: false,
        }
    }

    pub(crate) fn shared(&self) -> &'a EvalShared {
        self.shared
    }

    pub(crate) fn engine_ref(&self) -> &Engine {
        self.engine
    }

    pub(crate) fn value(&self, id: NodeId) -> Option<&Arc<NodeValue>> {
        self.values.get(&id)
    }

    /// A done node's transcript (a CT1 value carries its own).
    pub(crate) fn transcript(&self, id: NodeId) -> Option<&Transcript> {
        match self.values.get(&id).map(|value| value.as_ref()) {
            Some(NodeValue::Ct1(value)) => Some(&value.transcript),
            _ => self.transcripts.get(&id),
        }
    }

    /// Every done node.
    pub(crate) fn values(&self) -> impl Iterator<Item = (NodeId, &Arc<NodeValue>)> {
        self.values.iter().map(|(id, value)| (*id, value))
    }

    fn run(&mut self, id: NodeId, key: NodeKey) -> Result<Arc<NodeValue>, Missing> {
        self.stack.push(Frame {
            id,
            key: key.clone(),
            events: Vec::new(),
        });
        self.on_stack.insert(id);
        let shared = self.shared;
        let result = super::eval_host::body(shared, self, &key);
        let frame = self.stack.pop().expect("the frame pushed above");
        self.on_stack.remove(&id);
        let value = Arc::new(result?);
        self.computed += 1;
        if !matches!(key, NodeKey::Ct1(_)) && !frame.events.is_empty() {
            self.transcripts.insert(
                id,
                Transcript {
                    events: frame.events,
                },
            );
        }
        // The walk memoizes only the successes of these (a failure is
        // analyzed again at its next edge, in that edge's active chain:
        // a cycle report names it); the others memoize errors too.
        let memoized = value.error().is_none()
            || !matches!(
                key,
                NodeKey::Ct1(_)
                    | NodeKey::Src(_)
                    | NodeKey::Group(_)
                    | NodeKey::FlagScope(_)
                    | NodeKey::ExecCfg(..)
            );
        if memoized {
            self.values.insert(id, Arc::clone(&value));
        } else {
            if let NodeKey::Ct1(ct) = &key {
                self.first.canonical.remove(&ct.key);
            }
            self.failed.insert(id, Arc::clone(&value));
        }
        Ok(value)
    }
}

impl Cx for Inline<'_> {
    fn need(&mut self, keys: &[NodeKey]) -> Result<Vals, Missing> {
        let mut values = Vec::with_capacity(keys.len());
        for key in keys {
            let id = self.interner.intern(key);
            if let Some(value) = self.values.get(&id) {
                values.push(Arc::clone(value));
                continue;
            }
            if self.on_stack.contains(&id) {
                return Err(Missing::BackEdge(id));
            }
            values.push(self.run(id, key.clone())?);
        }
        Ok(Vals(values))
    }

    fn emit(&mut self, event: Event) {
        self.saw_flush |= event == Event::Flush;
        match self.stack.last_mut() {
            Some(frame) => frame.events.push(event),
            None => self.root_events.push(event),
        }
    }

    fn current(&self) -> NodeId {
        self.stack.last().expect("a node is being computed").id
    }
}

impl Phase1Cx for Inline<'_> {
    fn engine(&mut self) -> &mut Engine {
        self.engine
    }

    fn state(&mut self) -> &mut FirstVisit {
        &mut self.first
    }

    fn state_ref(&self) -> &FirstVisit {
        &self.first
    }

    fn done_value(&self, key: &NodeKey) -> Option<Arc<NodeValue>> {
        let id = self.interner.get(key)?;
        self.values.get(&id).cloned()
    }

    fn in_progress(&self, key: &NodeKey) -> bool {
        self.interner
            .get(key)
            .is_some_and(|id| self.on_stack.contains(&id))
    }

    fn active_ct1(&self) -> Vec<ConfiguredTargetKey> {
        let mut active = Vec::new();
        for frame in self.stack.iter().rev() {
            match &frame.key {
                NodeKey::Ct1(ct) => active.push(ct.key.clone()),
                // A detached analysis (Engine's fresh `active`): a
                // platform's tree, a registration.
                NodeKey::Platform(_) | NodeKey::RegToolchains(_) | NodeKey::RegExecPlatforms(_) => {
                    break;
                }
                _ => {}
            }
        }
        active.reverse();
        active
    }

    fn has_effects(&self, id: NodeId) -> bool {
        self.transcript(id)
            .is_some_and(|transcript| !transcript.events.is_empty())
    }

    fn tracking(&self) -> bool {
        self.tracking
    }

    fn intern(&mut self, key: &NodeKey) -> NodeId {
        self.interner.intern(key)
    }
}
