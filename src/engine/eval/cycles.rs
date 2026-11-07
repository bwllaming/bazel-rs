//! Cycles at quiescence (docs/EVALUATOR.md "Cycles and errors").
//!
//! A dependency cycle never completes: its nodes park waiting for each
//! other until the evaluator is quiescent (pool idle, no repository work
//! outstanding, roots not done). The coordinator then hands the
//! [`WaitGraph`] to a [`CycleHandler`] — pass 1's DFS replay in S1+
//! (worker C), [`DfsCycles`] here and in the tests — which answers with
//! **back edges**: `(parent, child)` pairs. The evaluator re-runs each
//! parent, and its `need` of `child` returns `Missing::BackEdge(child)`
//! (exactly what the inline driver answers when `child` is on the active
//! chain), so the body produces today's reentrancy error as a value
//! (`cycle_error`, `toolchain_cycle`, `execution_platform_cycle`). A pair
//! is answered at most once: a handler that finds nothing new stops the
//! evaluation, and the parked nodes are reported unresolved.
//!
//! Owner: worker A.

use crate::fast_hash::{FxHashMap, FxHashSet};

use super::graph::{Graph, State, Wait};
use super::key::NodeId;

/// The not-done part of the graph at quiescence.
pub(crate) struct WaitGraph<'g> {
    pub(crate) graph: &'g Graph,
    /// The evaluation's roots, in request order.
    pub(crate) roots: Vec<NodeId>,
    /// Every parked node and what it waits for.
    pub(crate) parked: FxHashMap<NodeId, Wait>,
    /// Nodes set aside in frontier-only mode.
    pub(crate) deferred: Vec<NodeId>,
    /// The pairs already answered with `BackEdge` (never answered twice).
    pub(crate) answered: FxHashSet<(NodeId, NodeId)>,
}

impl WaitGraph<'_> {
    /// What `id` waits on, in need order (empty unless parked on nodes).
    pub(crate) fn waits(&self, id: NodeId) -> &[NodeId] {
        self.parked.get(&id).map_or(&[], Wait::nodes)
    }

    pub(crate) fn is_done(&self, id: NodeId) -> bool {
        self.graph.entry(id).state() == State::Done
    }
}

/// What the handler decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// Re-run each parent; its `need` of the child answers `BackEdge`.
    Answer(Vec<(NodeId, NodeId)>),
    /// Nothing to answer: the evaluation stops; parked nodes are
    /// reported unresolved.
    Stop,
}

/// Called on the coordinator at quiescence with roots not done.
pub(crate) trait CycleHandler {
    fn at_quiescence(&mut self, graph: &WaitGraph<'_>) -> Resolution;
}

/// The first back edge of a depth-first search from the roots in order,
/// following each parked node's waits in need order (the walk's DFS over
/// the wait graph; pass 1 replaces it with the transcript replay).
#[derive(Default)]
pub(crate) struct DfsCycles {
    /// How many times it answered (tests).
    pub(crate) answers: usize,
}

impl CycleHandler for DfsCycles {
    fn at_quiescence(&mut self, graph: &WaitGraph<'_>) -> Resolution {
        match first_back_edge(graph) {
            Some(pair) => {
                self.answers += 1;
                Resolution::Answer(vec![pair])
            }
            None => Resolution::Stop,
        }
    }
}

/// The first unanswered back edge in DFS order (iterative: chains are
/// deep).
pub(crate) fn first_back_edge(graph: &WaitGraph<'_>) -> Option<(NodeId, NodeId)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        Active,
        Finished,
    }
    let mut marks: FxHashMap<NodeId, Mark> = FxHashMap::default();
    for &root in &graph.roots {
        if marks.contains_key(&root) || graph.is_done(root) {
            continue;
        }
        // (node, index of the next wait to follow)
        let mut stack: Vec<(NodeId, usize)> = vec![(root, 0)];
        marks.insert(root, Mark::Active);
        while let Some(&mut (node, ref mut next)) = stack.last_mut() {
            let waits = graph.waits(node);
            if *next == waits.len() {
                marks.insert(node, Mark::Finished);
                stack.pop();
                continue;
            }
            let child = waits[*next];
            *next += 1;
            match marks.get(&child) {
                Some(Mark::Active) => {
                    if !graph.answered.contains(&(node, child)) {
                        return Some((node, child));
                    }
                }
                Some(Mark::Finished) => {}
                None => {
                    if !graph.is_done(child) {
                        marks.insert(child, Mark::Active);
                        stack.push((child, 0));
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::eval::key::NodeKey;

    fn key(i: u32) -> NodeKey {
        NodeKey::Tgt(crate::label::Label {
            repository: String::new(),
            package: "c".into(),
            name: i.to_string(),
        })
    }

    fn wait_graph<'g>(graph: &'g Graph, edges: &[(u32, &[u32])], roots: &[u32]) -> WaitGraph<'g> {
        let ids: Vec<NodeId> = (0..10).map(|i| graph.intern(&key(i))).collect();
        WaitGraph {
            graph,
            roots: roots.iter().map(|&r| ids[r as usize]).collect(),
            parked: edges
                .iter()
                .map(|(from, to)| {
                    (
                        ids[*from as usize],
                        Wait::Nodes(to.iter().map(|&t| ids[t as usize]).collect()),
                    )
                })
                .collect(),
            deferred: Vec::new(),
            answered: FxHashSet::default(),
        }
    }

    #[test]
    fn finds_the_first_back_edge_in_dfs_order() {
        let graph = Graph::default();
        // 0 → 1 → 2 → 1 (cycle), 0 → 3 → 3 (self-cycle, later in order).
        let mut waits = wait_graph(
            &graph,
            &[(0, &[1, 3]), (1, &[2]), (2, &[1]), (3, &[3])],
            &[0],
        );
        let id = |i| graph.get(&key(i)).unwrap();
        assert_eq!(first_back_edge(&waits), Some((id(2), id(1))));
        waits.answered.insert((id(2), id(1)));
        assert_eq!(first_back_edge(&waits), Some((id(3), id(3))));
        waits.answered.insert((id(3), id(3)));
        assert_eq!(first_back_edge(&waits), None);
        let mut handler = DfsCycles::default();
        assert_eq!(handler.at_quiescence(&waits), Resolution::Stop);
    }

    #[test]
    fn an_acyclic_wait_graph_has_no_back_edge() {
        let graph = Graph::default();
        // A diamond waiting on a deferred leaf.
        let waits = wait_graph(&graph, &[(0, &[1, 2]), (1, &[3]), (2, &[3])], &[0, 1]);
        assert_eq!(first_back_edge(&waits), None);
    }

    #[test]
    fn deep_chains_do_not_recurse() {
        let graph = Graph::default();
        let ids: Vec<NodeId> = (0..100_000).map(|i| graph.intern(&key(i))).collect();
        let mut parked: FxHashMap<NodeId, Wait> = ids
            .windows(2)
            .map(|pair| (pair[0], Wait::Nodes(vec![pair[1]])))
            .collect();
        parked.insert(*ids.last().unwrap(), Wait::BackEdge(ids[0]));
        let waits = WaitGraph {
            graph: &graph,
            roots: vec![ids[0]],
            parked,
            deferred: Vec::new(),
            answered: FxHashSet::default(),
        };
        assert_eq!(
            first_back_edge(&waits),
            Some((*ids.last().unwrap(), ids[0]))
        );
    }
}
