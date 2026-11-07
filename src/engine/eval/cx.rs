//! The node-function context (docs/EVALUATOR.md "Discovery").
//!
//! A node body is a synchronous function of `&EvalShared` and a `&mut dyn
//! Cx`: it asks for the values it needs with [`Cx::need`], a batch of keys
//! per dependency phase (select() conditions; the target platform;
//! toolchains; dependency edges; toolchain implementations; native
//! prerequisites; config_setting prerequisites; aspect edges). When every
//! value is done, `need` returns them in key order; otherwise it returns
//! [`Missing`] and the body returns it at once (`?`): the attempt is
//! dropped (nothing it computed survives but its interior caches) and the
//! node is re-run from the start when what it waits for is done (restart
//! model; a compute-state is added only if restart CPU exceeds 20%).
//!
//! The inline driver (one thread) answers every `need` by running the
//! missing nodes immediately, depth first: today's DFS.
//!
//! Owner: worker A (the scheduler implements `Cx`). Frozen in S0.

use std::sync::Arc;

use crate::configuration::ConfigId;

use super::key::{NodeId, NodeKey};
use super::shared::EvalShared;
use super::transcript::Event;
use super::value::NodeValue;

/// Why a body cannot continue now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Missing {
    /// These nodes are not done: the node waits for all of them (the
    /// whole batch is scheduled at once).
    Nodes(Vec<NodeId>),
    /// A repository is not fetched (the sentinel of
    /// crate::repository_demand): the coordinator's repository lane
    /// fetches it, then the node is re-run.
    Repo(String),
    /// The configuration's deferred target platform is not resolved yet
    /// (today `platform_pending`/`deferred_target_platform`).
    TargetPlatform(ConfigId),
    /// A needed node is an ancestor of this one in the active chain
    /// (a reentrancy guard of the walk: `resolving_platforms`,
    /// `computing_toolchains`, `computing_execution_platforms`, the CT1
    /// `active` stack). Not an error by itself: the evaluator parks the
    /// node; at quiescence pass 1 replays the transcripts, finds the back
    /// edge in DFS order and reports today's cycle text (`cycle_error`,
    /// `toolchain_cycle`, `execution_platform_cycle`).
    BackEdge(NodeId),
}

/// The values of one [`Cx::need`] batch, in key order.
#[derive(Clone, Default)]
pub(crate) struct Vals(pub(crate) Vec<Arc<NodeValue>>);

impl Vals {
    pub(crate) fn get(&self, index: usize) -> &NodeValue {
        &self.0[index]
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &NodeValue> {
        self.0.iter().map(|value| &**value)
    }
}

/// What a body may do besides computing: need values, record events.
pub(crate) trait Cx {
    /// The values of `keys` when all are done; otherwise what is missing
    /// (every not-done key is scheduled). Needing the same key twice in
    /// one attempt is allowed and cheap.
    fn need(&mut self, keys: &[NodeKey]) -> Result<Vals, Missing>;

    /// One key ([`Self::need`]).
    fn need_one(&mut self, key: NodeKey) -> Result<Arc<NodeValue>, Missing> {
        let mut values = self.need(std::slice::from_ref(&key))?;
        Ok(values.0.swap_remove(0))
    }

    /// Record an effect in the node's transcript (dropped with a dropped
    /// attempt).
    fn emit(&mut self, event: Event);

    /// The node being computed.
    fn current(&self) -> NodeId;

    /// The id of `key` (interned on first sight): what events name a node
    /// by (`Reach`, `MemoUse`). Interface change (B, S1).
    fn id_of(&self, key: &NodeKey) -> NodeId;
}

/// A node body.
pub(crate) type NodeFn = fn(&EvalShared, &mut dyn Cx, &NodeKey) -> Result<NodeValue, Missing>;

#[cfg(test)]
pub(crate) mod tests {
    //! A map-backed `Cx` with the scheduler's contract (the S1 scheduler's
    //! tests and B's body tests use it).
    use super::*;
    use crate::fast_hash::FxHashMap;

    /// A context over a map of done values; `active` is the chain of
    /// nodes being computed (a needed key in it is a back edge).
    #[derive(Default)]
    pub(crate) struct MapCx {
        pub(crate) interner: super::super::key::Interner,
        pub(crate) done: FxHashMap<NodeId, Arc<NodeValue>>,
        pub(crate) active: Vec<NodeId>,
        pub(crate) events: Vec<Event>,
    }

    impl MapCx {
        pub(crate) fn finish(&mut self, key: &NodeKey, value: NodeValue) {
            let id = self.interner.intern(key);
            self.done.insert(id, Arc::new(value));
        }
    }

    impl Cx for MapCx {
        fn need(&mut self, keys: &[NodeKey]) -> Result<Vals, Missing> {
            let ids: Vec<NodeId> = keys.iter().map(|key| self.interner.intern(key)).collect();
            if let Some(back) = ids.iter().find(|id| self.active.contains(id)) {
                return Err(Missing::BackEdge(*back));
            }
            let missing: Vec<NodeId> = ids
                .iter()
                .filter(|id| !self.done.contains_key(id))
                .copied()
                .collect();
            if !missing.is_empty() {
                return Err(Missing::Nodes(missing));
            }
            Ok(Vals(
                ids.iter().map(|id| Arc::clone(&self.done[id])).collect(),
            ))
        }

        fn emit(&mut self, event: Event) {
            self.events.push(event);
        }

        fn current(&self) -> NodeId {
            *self.active.last().expect("a node is being computed")
        }

        fn id_of(&self, key: &NodeKey) -> NodeId {
            self.interner.intern(key)
        }
    }

    fn label(text: &str) -> crate::label::Label {
        crate::label::Label::parse_canonical(text).unwrap()
    }

    #[test]
    fn need_batches_missing_keys_and_returns_values_in_key_order() {
        let mut cx = MapCx::default();
        let (a, b) = (NodeKey::Src(label("//a:x")), NodeKey::Src(label("//b:y")));
        cx.finish(&a, NodeValue::Src(Ok(())));
        let Err(Missing::Nodes(missing)) = cx.need(&[a.clone(), b.clone()]) else {
            panic!("b is not done");
        };
        assert_eq!(missing, [cx.interner.get(&b).unwrap()]);
        cx.finish(&b, NodeValue::Src(Err("hidden".into())));
        let values = cx.need(&[b.clone(), a.clone()]).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values.get(0).error().as_deref(), Some("hidden"));
        assert_eq!(values.get(1).error(), None);
        assert_eq!(cx.need_one(a).unwrap().error(), None);
    }

    #[test]
    fn an_active_ancestor_is_a_back_edge() {
        let mut cx = MapCx::default();
        let key = NodeKey::Platform(crate::config_registry::ConfiguredTargetKey::new(
            label("//p:p"),
            crate::configuration::Configuration::default().id,
        ));
        let id = cx.interner.intern(&key);
        cx.active.push(id);
        assert_eq!(cx.need(&[key]).err(), Some(Missing::BackEdge(id)));
        cx.emit(Event::Warn("w".into()));
        assert_eq!(cx.current(), id);
        assert_eq!(cx.events.len(), 1);
    }
}
