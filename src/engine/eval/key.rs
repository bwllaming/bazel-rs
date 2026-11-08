//! Node keys and their interning (docs/EVALUATOR.md "Keys").
//!
//! Every value the evaluator computes is a node named by a [`NodeKey`];
//! the graph refers to nodes by a dense [`NodeId`] the [`Interner`]
//! assigns on first sight (sharded key → id index, append-only id → key
//! arena: lock-free reads by id, one shard lock per new key).
//!
//! Owner: worker A (the graph). Frozen in S0: adding a variant is a
//! reviewed interface change (B and C match on it).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::aspects::AspectApplicationKey;
use crate::configuration::ConfigId;
use crate::label::{Label, PackageId};

use super::super::{ConfiguredTargetKey, TransitionCacheKey};

/// A configured target's phase-1/phase-2 node key: today's
/// [`ConfiguredTargetKey`] plus the two facts the sequential walk reads
/// from first-visit state instead of from the key (the hidden first-visit
/// memos, docs/EVALUATOR.md "CtKey normalization"):
///
/// - `platform_tree`: the target is analyzed inside a platform's tree
///   (today `!Engine::resolving_platforms.is_empty()`): the empty
///   configuration — no toolchain resolution, no exec transition, and an
///   entry in `empty_configuration_rules`.
/// - `forced_exec`: the target is a selected toolchain implementation of a
///   parent whose execution platform is this label (today
///   `Engine::toolchain_implementations`, read by `toolchain_context_key`
///   as `ToolchainContextKey::forced_exec`).
///
/// Normalization ([`CtKey::normalized`]): both facts are dropped when the
/// target's analysis cannot observe them, so that the common case keeps
/// one node per ConfiguredTargetKey:
///
/// - `platform_tree` is dropped in the no-config configuration (which is
///   empty anyway). It must otherwise be kept for **every** rule of a
///   platform's tree, not only for rules that observe the empty
///   configuration themselves: the context propagates to the rule's
///   dependencies (a filegroup's genrule dependency would otherwise escape
///   the empty configuration). [`Observes::exec_edges`] = `true` keeps it;
///   the S1 bodies pass that for every rule (`eval_host::edge_ct_key`). A
///   rule-local rule would need the transitive closure of what observes
///   it (S2 if the extra variants ever matter).
/// - `forced_exec` is kept only when the rule uses toolchain resolution
///   (the only reader is the toolchain context key) and the target is not
///   in a platform tree.
///
/// The sequential walk's first-visit semantics are reproduced by pass 1:
/// when two variants of one ConfiguredTargetKey are reached, the variant
/// reached first in DFS order is the target's configured node and plan
/// entry; later reaches are memo hits of it (docs/EVALUATOR.md).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CtKey {
    pub(crate) key: ConfiguredTargetKey,
    pub(crate) platform_tree: bool,
    pub(crate) forced_exec: Option<Label>,
}

/// What [`CtKey::normalized`] needs to know of the target's rule.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Observes {
    /// The configuration is the no-config configuration.
    pub(crate) no_config: bool,
    /// The rule uses toolchain resolution (`uses_toolchain_resolution`).
    pub(crate) toolchain_resolution: bool,
    /// The rule fails in an empty configuration (genrule, Starlark test
    /// rule, an exec-transition edge without `license_kinds`).
    pub(crate) exec_edges: bool,
}

impl CtKey {
    /// The plain key: outside a platform tree, no forced platform.
    pub(crate) fn plain(key: ConfiguredTargetKey) -> Self {
        Self {
            key,
            platform_tree: false,
            forced_exec: None,
        }
    }

    /// The key with the facts the target cannot observe dropped (the
    /// normalization rule above).
    pub(crate) fn normalized(mut self, observes: Observes) -> Self {
        if observes.no_config || !(observes.toolchain_resolution || observes.exec_edges) {
            self.platform_tree = false;
        }
        if !observes.toolchain_resolution || self.platform_tree || observes.no_config {
            self.forced_exec = None;
        }
        self
    }
}

/// The key of a node (docs/EVALUATOR.md "Nodes").
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NodeKey {
    /// A loaded package (today `Engine::packages`, crate::prefetch).
    Pkg(PackageId),
    /// A target lookup (`Engine::target`): rule, output file or source
    /// file, with its filesystem probes.
    Tgt(Label),
    /// A source file's analysis: its visibility check (today
    /// `Engine::analyzed_sources`).
    Src(Label),
    /// A package group's resolution (`Engine::resolve_group`).
    Group(Label),
    /// Phase 1 of a configured target: its configured node, rule,
    /// toolchain context, exec groups and configurations, edge
    /// configurations, delegation, and its transcript.
    Ct1(CtKey),
    /// The matching of a `config_setting` in a configuration
    /// (`ConfigMatching`), which may need phase-2 values of Starlark
    /// flags: replaces `plan_pending` flushes.
    CsMatch(CtKey),
    /// PlatformValue (`Engine::platforms`).
    Platform(ConfiguredTargetKey),
    /// RegisteredToolchainsValue per configuration.
    RegToolchains(ConfigId),
    /// RegisteredExecutionPlatformsValue per configuration.
    RegExecPlatforms(ConfigId),
    /// The modules' `register_execution_platforms` patterns (once).
    ModuleExecPlatforms,
    /// The exec configuration of (configuration, execution platform).
    ExecCfg(ConfigId, Label),
    /// An applied Starlark transition with its print() lines.
    Transition(TransitionCacheKey),
    /// StarlarkBuildSettingsDetailsValue of a transition's settings.
    TransitionDetails(Vec<Label>),
    /// Whether a Starlark flag is target-scoped.
    FlagScope(Label),
    /// An aspect's `propagation_predicate` on a target (label, aspect).
    AspectPred(Label, String),
    /// An aspect's function `attr_aspects`/`toolchains_aspects` lists on
    /// a configured target.
    AspectAttrAspects(ConfiguredTargetKey, String),
    /// An aspect's own toolchain contexts (configuration, aspect).
    AspectToolchains(ConfigId, String),
    /// The invocation's workspace status (computed once).
    Status,
    /// Phase 2 of a configured target: prepare, compute and its commit as
    /// a delta (natives included).
    Ct2(CtKey),
    /// The dependency view of an edge key (`DependencyViews`).
    View(ConfiguredTargetKey),
    /// One aspect application.
    Aspect(AspectApplicationKey),
}

impl NodeKey {
    /// A short name of the variant (diagnostics, PERF lines).
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            NodeKey::Pkg(_) => "Pkg",
            NodeKey::Tgt(_) => "Tgt",
            NodeKey::Src(_) => "Src",
            NodeKey::Group(_) => "Group",
            NodeKey::Ct1(_) => "CT1",
            NodeKey::CsMatch(_) => "CsMatch",
            NodeKey::Platform(_) => "Platform",
            NodeKey::RegToolchains(_) => "RegToolchains",
            NodeKey::RegExecPlatforms(_) => "RegExecPlatforms",
            NodeKey::ModuleExecPlatforms => "ModuleExecPlatforms",
            NodeKey::ExecCfg(..) => "ExecCfg",
            NodeKey::Transition(_) => "Transition",
            NodeKey::TransitionDetails(_) => "TransitionDetails",
            NodeKey::FlagScope(_) => "FlagScope",
            NodeKey::AspectPred(..) => "AspectPred",
            NodeKey::AspectAttrAspects(..) => "AspectAttrAspects",
            NodeKey::AspectToolchains(..) => "AspectToolchains",
            NodeKey::Status => "Status",
            NodeKey::Ct2(_) => "CT2",
            NodeKey::View(_) => "View",
            NodeKey::Aspect(_) => "Aspect",
        }
    }

    /// Whether the node runs on the coordinator only (repository work,
    /// the workspace status script): never on a pool thread. A flag's CT2
    /// runs on the pool speculatively (S2, the CsMatch split) or in the
    /// sequential pass.
    pub(crate) fn coordinator_only(&self) -> bool {
        matches!(self, NodeKey::Status | NodeKey::ModuleExecPlatforms)
    }
}

/// A node's dense id (assigned in first-sight order; never reused within
/// an evaluation). Ids are not deterministic across thread counts: output
/// order must never follow them (the linearizer orders by pass 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeId(pub(crate) u32);

/// Shards of the key → id index (a power of two).
const SHARDS: usize = 64;
/// Segment `i` of the arena holds `FIRST_SEGMENT << i` slots.
const FIRST_SEGMENT: usize = 1024;
/// Segments: room for `FIRST_SEGMENT * (2^SEGMENTS - 1)` ids (> 4·10^12;
/// the id space, u32, ends first).
const SEGMENTS: usize = 32;

/// One arena segment: its slots, each set once.
type Segment = Box<[OnceLock<NodeKey>]>;

/// The key ↔ id interner: a sharded index (key → id, one mutex per shard,
/// taken once per new key and per lookup) and an append-only arena (id →
/// key: segmented, lock-free reads).
pub(crate) struct Interner {
    shards: Box<[Mutex<crate::fast_hash::FxHashMap<NodeKey, NodeId>>]>,
    segments: Box<[OnceLock<Segment>]>,
    next: AtomicU32,
}

impl Default for Interner {
    fn default() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            segments: (0..SEGMENTS).map(|_| OnceLock::new()).collect(),
            next: AtomicU32::new(0),
        }
    }
}

/// The arena position of an id: (segment, offset).
pub(super) fn slot_of(id: u32) -> (usize, usize) {
    // Segment i starts at FIRST_SEGMENT * (2^i - 1).
    let scaled = id as usize / FIRST_SEGMENT + 1;
    let segment = (usize::BITS - 1 - scaled.leading_zeros()) as usize;
    let start = FIRST_SEGMENT * ((1 << segment) - 1);
    (segment, id as usize - start)
}

impl Interner {
    fn shard(&self, key: &NodeKey) -> &Mutex<crate::fast_hash::FxHashMap<NodeKey, NodeId>> {
        use std::hash::BuildHasher;
        let hash = crate::fast_hash::FxBuildHasher::default().hash_one(key);
        &self.shards[(hash >> 32) as usize & (SHARDS - 1)]
    }

    /// The id of `key`, assigned on first sight.
    pub(crate) fn intern(&self, key: &NodeKey) -> NodeId {
        let mut shard = self.shard(key).lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = shard.get(key) {
            return *id;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(id < u32::MAX, "internal error: node id space exhausted");
        let (segment, offset) = slot_of(id);
        let slots = self.segments[segment].get_or_init(|| {
            (0..FIRST_SEGMENT << segment)
                .map(|_| OnceLock::new())
                .collect()
        });
        // Published before the id escapes the shard lock.
        let _ = slots[offset].set(key.clone());
        let id = NodeId(id);
        shard.insert(key.clone(), id);
        id
    }

    /// The id of `key` if interned.
    pub(crate) fn get(&self, key: &NodeKey) -> Option<NodeId> {
        self.shard(key)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .copied()
    }

    /// The key of `id` (lock-free).
    pub(crate) fn key(&self, id: NodeId) -> &NodeKey {
        let (segment, offset) = slot_of(id.0);
        self.segments[segment]
            .get()
            .and_then(|slots| slots[offset].get())
            .expect("internal error: an id the interner never assigned")
    }

    /// The number of interned keys.
    pub(crate) fn len(&self) -> usize {
        self.next.load(Ordering::Relaxed) as usize
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    fn ct(text: &str) -> CtKey {
        CtKey::plain(ConfiguredTargetKey::new(
            label(text),
            crate::configuration::Configuration::default().id,
        ))
    }

    #[test]
    fn slots_cover_the_id_space_without_overlap() {
        assert_eq!(slot_of(0), (0, 0));
        assert_eq!(slot_of(1023), (0, 1023));
        assert_eq!(slot_of(1024), (1, 0));
        assert_eq!(slot_of(3071), (1, 2047));
        assert_eq!(slot_of(3072), (2, 0));
        let (segment, offset) = slot_of(u32::MAX - 1);
        assert!(segment < SEGMENTS);
        assert!(offset < FIRST_SEGMENT << segment);
    }

    #[test]
    fn interning_is_stable_and_reversible() {
        let interner = Interner::default();
        let a = NodeKey::Tgt(label("//a:a"));
        let b = NodeKey::Ct1(ct("//a:a"));
        let ia = interner.intern(&a);
        let ib = interner.intern(&b);
        assert_ne!(ia, ib);
        assert_eq!(interner.intern(&a), ia);
        assert_eq!(interner.get(&b), Some(ib));
        assert_eq!(interner.key(ia), &a);
        assert_eq!(interner.key(ib), &b);
        assert_eq!(interner.get(&NodeKey::Status), None);
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn concurrent_interning_assigns_one_id_per_key() {
        let interner = Interner::default();
        let keys: Vec<NodeKey> = (0..5000)
            .map(|i| NodeKey::Tgt(label(&format!("//p:t{i}"))))
            .collect();
        let ids: Vec<Vec<NodeId>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| keys.iter().map(|key| interner.intern(key)).collect()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(ids.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(interner.len(), keys.len());
        for (key, id) in keys.iter().zip(&ids[0]) {
            assert_eq!(interner.key(*id), key);
        }
    }

    #[test]
    fn normalization_drops_unobservable_facts() {
        let mut key = ct("//a:a");
        key.platform_tree = true;
        key.forced_exec = Some(label("//p:exec"));
        // A plain rule observes neither.
        assert_eq!(key.clone().normalized(Observes::default()), ct("//a:a"));
        // A toolchain-resolving rule observes the platform tree, and inside
        // one no forced platform.
        let toolchains = Observes {
            toolchain_resolution: true,
            ..Observes::default()
        };
        let normalized = key.clone().normalized(toolchains);
        assert!(normalized.platform_tree);
        assert_eq!(normalized.forced_exec, None);
        // Outside a platform tree it observes the forced platform.
        let mut outside = key.clone();
        outside.platform_tree = false;
        assert_eq!(
            outside.normalized(toolchains).forced_exec,
            Some(label("//p:exec"))
        );
        // A genrule observes the platform tree (its exec edge fails there)
        // but not a forced platform.
        let genrule = Observes {
            exec_edges: true,
            ..Observes::default()
        };
        let normalized = key.clone().normalized(genrule);
        assert!(normalized.platform_tree);
        assert_eq!(normalized.forced_exec, None);
        // The no-config configuration is empty anyway.
        let no_config = Observes {
            no_config: true,
            toolchain_resolution: true,
            exec_edges: true,
        };
        assert_eq!(key.normalized(no_config), ct("//a:a"));
    }
}
