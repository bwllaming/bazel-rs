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
    /// Nodes computed, in completion order.
    pub(crate) computed: usize,
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

    pub(crate) fn transcript(&self, id: NodeId) -> Option<&Transcript> {
        self.transcripts.get(&id)
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
        self.values.insert(id, Arc::clone(&value));
        self.transcripts.insert(
            id,
            Transcript {
                events: frame.events,
            },
        );
        self.computed += 1;
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
        self.transcripts
            .get(&id)
            .is_some_and(|transcript| !transcript.events.is_empty())
    }

    fn intern(&mut self, key: &NodeKey) -> NodeId {
        self.interner.intern(key)
    }
}
