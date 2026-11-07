//! Pass 1's cycle handler (docs/EVALUATOR.md "Cycles and errors"): at
//! quiescence the parked part of the graph is replayed depth first from
//! the roots in need order — the walk's DFS — with the walk's two kinds of
//! reentrancy check:
//!
//! - a CT1 node is a back edge only when it is active in the **current
//!   chain** (the walk's `active` stack, `cycle_error`); the memo nodes
//!   that start a fresh chain (platform resolution, registered toolchains
//!   and execution platforms: the walk's `analyze(…, &mut Vec::new(), …)`)
//!   open a new one, so a CT1 node active only outside it is entered again,
//!   as the walk analyzes it again;
//! - any other node is a back edge when it is active anywhere (the walk's
//!   global guards `resolving_platforms`, `computing_toolchains`,
//!   `computing_execution_platforms`).
//!
//! The first unanswered back edge `(parent, child)` in that order is the
//! answer: the parent re-runs and its `need` of the child returns
//! `BackEdge`, so the body words today's error (`cycle_error`,
//! `toolchain_cycle`, `execution_platform_cycle`). Done nodes are not
//! entered (a parked node is reachable from the roots only through parked
//! nodes). Iterative: chains are deep.
//!
//! It replaces worker A's stand-in `cycles::DfsCycles` (which treats every
//! active node as a back edge, whatever chain it is in).

use crate::fast_hash::{FxHashMap, FxHashSet};

use super::super::cycles::{CycleHandler, Resolution, WaitGraph};
use super::super::key::{NodeId, NodeKey};

/// The walk-order cycle handler.
#[derive(Default)]
pub(crate) struct Pass1Cycles {
    /// How many times it answered (tests, diagnostics).
    pub(crate) answers: usize,
}

impl CycleHandler for Pass1Cycles {
    fn at_quiescence(&mut self, graph: &WaitGraph<'_>) -> Resolution {
        match walk_back_edge(graph) {
            Some(pair) => {
                self.answers += 1;
                Resolution::Answer(vec![pair])
            }
            None => Resolution::Stop,
        }
    }
}

/// A memo node whose computation starts a fresh active chain.
fn opens_chain(key: &NodeKey) -> bool {
    matches!(
        key,
        NodeKey::Platform(_) | NodeKey::RegToolchains(_) | NodeKey::RegExecPlatforms(_)
    )
}

/// The first unanswered back edge in the walk's order.
pub(crate) fn walk_back_edge(graph: &WaitGraph<'_>) -> Option<(NodeId, NodeId)> {
    struct Frame {
        node: NodeId,
        next: usize,
        /// The chain the node's own reaches go to.
        chain: usize,
        opened: bool,
    }
    let mut finished: FxHashSet<NodeId> = FxHashSet::default();
    for &root in &graph.roots {
        if finished.contains(&root) || graph.is_done(root) {
            continue;
        }
        // The CT1 nodes active in each chain, outermost first.
        let mut chains: Vec<FxHashSet<NodeId>> = vec![FxHashSet::default()];
        // Every node's activations (any chain).
        let mut active: FxHashMap<NodeId, u32> = FxHashMap::default();
        let mut stack: Vec<Frame> = Vec::new();
        let enter = |node: NodeId,
                     chains: &mut Vec<FxHashSet<NodeId>>,
                     active: &mut FxHashMap<NodeId, u32>,
                     stack: &mut Vec<Frame>| {
            let key = graph.graph.key(node);
            let opened = opens_chain(key);
            if opened {
                chains.push(FxHashSet::default());
            } else if matches!(key, NodeKey::Ct1(_)) {
                chains.last_mut().expect("a chain").insert(node);
            }
            *active.entry(node).or_default() += 1;
            stack.push(Frame {
                node,
                next: 0,
                chain: chains.len() - 1,
                opened,
            });
        };
        enter(root, &mut chains, &mut active, &mut stack);
        while let Some(frame) = stack.last_mut() {
            let waits = graph.waits(frame.node);
            if frame.next == waits.len() {
                let frame = stack.pop().expect("a frame");
                if frame.opened {
                    chains.pop();
                } else {
                    chains[frame.chain].remove(&frame.node);
                }
                let count = active.get_mut(&frame.node).expect("active");
                *count -= 1;
                if *count == 0 {
                    active.remove(&frame.node);
                    finished.insert(frame.node);
                }
                continue;
            }
            let (node, chain) = (frame.node, frame.chain);
            let child = waits[frame.next];
            frame.next += 1;
            if graph.is_done(child) || finished.contains(&child) {
                continue;
            }
            let back = match graph.graph.key(child) {
                NodeKey::Ct1(_) => chains[chain].contains(&child),
                _ => active.contains_key(&child),
            };
            if back {
                if !graph.answered.contains(&(node, child)) {
                    return Some((node, child));
                }
                continue;
            }
            enter(child, &mut chains, &mut active, &mut stack);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::eval::cycles::DfsCycles;
    use crate::engine::eval::graph::{Graph, Wait};
    use crate::engine::eval::key::CtKey;
    use crate::engine::eval::linearize::pass1::tests::ctk;

    fn ct1(name: &str) -> NodeKey {
        NodeKey::Ct1(CtKey::plain(ctk(name)))
    }

    fn waits<'g>(
        graph: &'g Graph,
        edges: &[(&NodeKey, &[&NodeKey])],
        roots: &[&NodeKey],
    ) -> WaitGraph<'g> {
        WaitGraph {
            graph,
            roots: roots.iter().map(|key| graph.intern(key)).collect(),
            parked: edges
                .iter()
                .map(|(from, to)| {
                    (
                        graph.intern(from),
                        Wait::Nodes(to.iter().map(|key| graph.intern(key)).collect()),
                    )
                })
                .collect(),
            deferred: Vec::new(),
            answered: FxHashSet::default(),
        }
    }

    #[test]
    fn a_ct1_cycle_is_answered_in_dfs_order() {
        let graph = Graph::default();
        let (a, b, c, d) = (ct1("a"), ct1("b"), ct1("c"), ct1("d"));
        let mut wait = waits(
            &graph,
            &[(&a, &[&b, &d]), (&b, &[&c]), (&c, &[&b]), (&d, &[&d])],
            &[&a],
        );
        let id = |key: &NodeKey| graph.get(key).unwrap();
        assert_eq!(walk_back_edge(&wait), Some((id(&c), id(&b))));
        wait.answered.insert((id(&c), id(&b)));
        assert_eq!(walk_back_edge(&wait), Some((id(&d), id(&d))));
        wait.answered.insert((id(&d), id(&d)));
        let mut handler = Pass1Cycles::default();
        assert_eq!(handler.at_quiescence(&wait), Resolution::Stop);
    }

    /// a → platform p → b → a: inside p's fresh chain, a is not active,
    /// so the walk analyzes it again and meets p's reentrancy guard: the
    /// back edge is (a, p), not (b, a) as a plain DFS over the waits says.
    #[test]
    fn a_memo_starts_a_fresh_chain() {
        let graph = Graph::default();
        let (a, b) = (ct1("a"), ct1("b"));
        let p = NodeKey::Platform(ctk("p"));
        let wait = waits(&graph, &[(&a, &[&p]), (&p, &[&b]), (&b, &[&a])], &[&a]);
        let id = |key: &NodeKey| graph.get(key).unwrap();
        assert_eq!(walk_back_edge(&wait), Some((id(&a), id(&p))));
        assert_eq!(
            DfsCycles::default().at_quiescence(&wait),
            Resolution::Answer(vec![(id(&b), id(&a))])
        );
    }

    #[test]
    fn deep_chains_do_not_recurse() {
        let graph = Graph::default();
        let keys: Vec<NodeKey> = (0..100_000).map(|i| ct1(&format!("n{i}"))).collect();
        let ids: Vec<NodeId> = keys.iter().map(|key| graph.intern(key)).collect();
        let mut parked: FxHashMap<NodeId, Wait> = ids
            .windows(2)
            .map(|pair| (pair[0], Wait::Nodes(vec![pair[1]])))
            .collect();
        parked.insert(*ids.last().unwrap(), Wait::Nodes(vec![ids[0]]));
        let wait = WaitGraph {
            graph: &graph,
            roots: vec![ids[0]],
            parked,
            deferred: Vec::new(),
            answered: FxHashSet::default(),
        };
        assert_eq!(walk_back_edge(&wait), Some((*ids.last().unwrap(), ids[0])));
    }
}
