//! W0-A retained-memory breakdown at the end of phase 2
//! (`BAZEL_RS_TIMING_ALLOC=1`; docs/PERF_LAYER1.md "PERF-ALLOC").
//!
//! Right after phase-2 action planning (where the `PERF-STAGE` report is
//! printed), the main planning structures are measured and printed on
//! stderr, largest first:
//!
//! ```text
//! PERF-ALLOC <bytes> <name>                      (one line per bucket)
//! PERF-ALLOC-DETAIL <bytes> <bucket>/part:<part>  (rule parts inside a bucket)
//! PERF-ALLOC-HEAP <bytes> <group> heaps=<n>      (frozen-heap arenas by referrer)
//! PERF-ALLOC-SIZES rule=<bytes> ...              (inline sizes of key types)
//! PERF-ALLOC-COUNTS packages=<n> rules=<n> ...   (entries of the main structures)
//! PERF-ALLOC-SUMMARY total=<bytes> rss=<kB>kB hwm=<kB>kB covered=<pct>% rss_anon=… rss_file=… threads=… elapsed=<s>s
//! ```
//!
//! Method. A single `allocative::FlameGraphBuilder` holds the one
//! "already counted" set of the measurement, so a shared allocation (an
//! `Arc` pointee, a Starlark frozen heap) is counted exactly once:
//! - Engine-native structures (`packages`, `plan`, `configured_rules`,
//!   `configured_nodes`, every `action_planning.*` map — since perf-w1
//!   (integration-20) with the memoized dependency views and the File each
//!   Artifact caches —, the per-target toolchain/exec-group contexts and a
//!   few other `engine.*` maps) are
//!   walked by [`Footprint`] in this module:
//!   arithmetic over capacities, `BTreeMap` nodes estimated at ~8 entries
//!   per 11-slot node, hash tables from their capacity. An `Arc` pointee is
//!   attributed to the first bucket that reaches it, in the order of
//!   [`GROUPS`] (`packages` first, so rules shared with the plan count
//!   there).
//! - Starlark-side data is measured by its own `Allocative`
//!   implementations: every frozen heap reachable from the structures
//!   (arena chunks plus the Rust allocations its values own), depset
//!   nodes, runfiles and `Target` values. allocative attributes a shared
//!   pointee to its type, so these appear as `shared:<Type>` buckets
//!   (`shared:FrozenFrozenHeap`, `shared:DepsetNode`, `shared:Target`, ...).
//!   `PERF-ALLOC-HEAP` splits the frozen-heap arena bytes (only the arena
//!   chunks) by the first structure that references each heap, the
//!   `.bzl` module cache first (`bzl_modules`).
//! - `native.malloc_overhead_est`: glibc chunk rounding and headers of the
//!   allocations the native walk counted (an estimate: 16-byte granules,
//!   8-byte header, 32-byte minimum; page-rounded above the mmap threshold).
//! - `PERF-ALLOC-DETAIL` splits the `Rule` data inside a bucket by part
//!   (attribute schema, attribute values, declaration, ctx surface,
//!   toolchain contexts, function handles, native attributes, location;
//!   already included in the bucket's line). A rule's inline size (its
//!   share of the containing buffer) stays in the plain total.
//!
//! Coverage and known gaps (the summary's `covered` is total / VmRSS):
//! - not walked: the package loader's other caches, the repository set
//!   (mappings), configuration registry, registered toolchains and
//!   platforms, aspect definitions, the action cache, freed-but-retained
//!   allocator memory (phase-1/phase-2 churn, fragmentation) and the
//!   binary (file-backed, `rss_file` in the summary);
//! - shallow (inline size only): `Arc`s of `CtxSurface`,
//!   `ToolchainContextData`, `ExecGroupsData`, `PlatformInfo`,
//!   `DeclaredToolchainInfo`, package groups; `PackageArgs`,
//!   `ActionExtra::transform`, deferred action callbacks, workspace status;
//! - Starlark values whose derives mark fields `#[allocative(skip)]`
//!   (`DefaultInfo` files/runfiles, `Target` runfiles reached through a
//!   heap) are counted only where another structure reaches the same data
//!   (the `runfiles_info` map holds the runfiles DAGs).
//!
//! Cost: one walk over everything (seconds on large builds) and the
//! builder's tree, done only when enabled; timings and VmHWM of a run with
//! the switch on are not comparable to a plain timing run.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use allocative::{Allocative, FlameGraphBuilder, Key, Visitor};
use starlark::values::{FrozenHeapRef, OwnedFrozenValue};

use super::{
    ActionPlanning, Artifact, ConfiguredNode, ConfiguredNodes, Engine, OutputProducer, Plan,
    RunfilesNodes,
};
use crate::config_registry::ConfiguredTargetKey;
use crate::configuration::ConfigId;
use crate::label::{Label, PackageId};
use crate::package::{Generator, MacroOwner, Package, Rule};
use crate::starlark_types::File;
use crate::visibility::Visibility;

/// `BAZEL_RS_TIMING_ALLOC` set to anything but empty or `0`
/// ([`crate::diagnostic_switch`], the rule of `BAZEL_RS_TIMING` too).
pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| crate::diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING_ALLOC")))
}

/// The engine hook (end of phase 2 in `plan_build`): a no-op unless
/// [`enabled`].
pub(super) fn report_if_enabled(
    engine: &Engine,
    state: &ActionPlanning,
    plan: &Plan,
    configured: &ConfiguredNodes,
) {
    if !enabled() {
        return;
    }
    let started = std::time::Instant::now();
    let status = crate::proc_self_status();
    let rss_kb: u64 = crate::proc_status_value(&status, "VmRSS:")
        .parse()
        .unwrap_or(0);
    let hwm_kb: u64 = crate::proc_status_value(&status, "VmHWM:")
        .parse()
        .unwrap_or(0);
    // Anonymous vs file-backed resident memory and the thread count, to
    // read the summary's coverage against.
    let anon_kb = crate::proc_status_value(&status, "RssAnon:").to_owned();
    let file_kb = crate::proc_status_value(&status, "RssFile:").to_owned();
    let threads = crate::proc_status_value(&status, "Threads:").to_owned();
    let census = HeapCensus::collect(engine, state, plan);
    let groups = groups(engine, state, plan, configured);
    let (text, overhead) = measure(&census.postorder, &groups);
    let (mut buckets, details) = buckets(&text, &groups);
    buckets.push((MALLOC_OVERHEAD.to_owned(), overhead as u64));
    sort_rows(&mut buckets);
    let total: u64 = buckets.iter().map(|(_, bytes)| *bytes).sum();
    for (name, bytes) in &buckets {
        eprintln!("PERF-ALLOC {bytes} {name}");
    }
    for (name, bytes) in &details {
        eprintln!("PERF-ALLOC-DETAIL {bytes} {name}");
    }
    let mut heaps: Vec<(u64, &str, u64)> = census
        .groups
        .iter()
        .map(|(name, (bytes, count))| (*bytes, *name, *count))
        .collect();
    heaps.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    for (bytes, name, count) in heaps {
        eprintln!("PERF-ALLOC-HEAP {bytes} {name} heaps={count}");
    }
    eprintln!(
        "PERF-ALLOC-SIZES rule={} custom_invocation={} configured_target_key={} label={} artifact={} file={} planned_action={} configured_node={} target_runfiles={} target={}",
        size_of::<Rule>(),
        size_of::<crate::custom_rule::CustomInvocation>(),
        size_of::<ConfiguredTargetKey>(),
        size_of::<Label>(),
        size_of::<Artifact>(),
        size_of::<File>(),
        size_of::<crate::scheduler::PlannedAction>(),
        size_of::<ConfiguredNode>(),
        size_of::<crate::runfiles::TargetRunfiles>(),
        size_of::<crate::starlark_types::Target>(),
    );
    eprintln!(
        "PERF-ALLOC-COUNTS packages={} rules={} plan={} configured_rules={} configured_nodes={} dry={} artifact_index={} views={} nodes={} target_identities={} heaps={}",
        engine.packages.len(),
        engine
            .packages
            .values()
            .map(|package| package.rules.len())
            .sum::<usize>(),
        plan.len(),
        engine.configured_rules.len(),
        configured.len(),
        state.dry.len(),
        state.artifact_index.len(),
        state.views.len(),
        state.nodes.len(),
        state.target_identities.len(),
        census.postorder.len(),
    );
    let covered = if rss_kb > 0 {
        total as f64 * 100.0 / (rss_kb as f64 * 1024.0)
    } else {
        0.0
    };
    eprintln!(
        "PERF-ALLOC-SUMMARY total={total} rss={rss_kb}kB hwm={hwm_kb}kB covered={covered:.1}% rss_anon={anon_kb}kB rss_file={file_kb}kB threads={threads} elapsed={:.3}s",
        started.elapsed().as_secs_f64()
    );
}

/// One named root of the measurement, in attribution order.
type Group<'a> = (&'static str, &'a dyn Footprint);

/// The measured structures, in attribution order (an `Arc` shared by two
/// of them counts toward the first).
fn groups<'a>(
    engine: &'a Engine,
    state: &'a ActionPlanning,
    plan: &'a Plan,
    configured: &'a ConfiguredNodes,
) -> Vec<Group<'a>> {
    vec![
        ("packages", &engine.packages),
        ("plan", plan),
        ("configured_rules", &engine.configured_rules),
        ("configured_nodes", configured),
        ("action_planning.dry", &state.dry),
        ("action_planning.custom_providers", &state.custom_providers),
        ("action_planning.provider_keys", &state.provider_keys),
        ("action_planning.runfiles_info", &state.runfiles_info),
        ("action_planning.artifact_index", &state.artifact_index),
        ("action_planning.declared", &state.declared),
        ("action_planning.producers", &state.producers),
        ("action_planning.nodes", &state.nodes),
        ("action_planning.source_inputs", &state.source_inputs),
        ("action_planning.input_groups", &state.input_groups),
        ("action_planning.delegations", &state.delegations),
        (
            "action_planning.native_output_groups",
            &state.native_output_groups,
        ),
        ("action_planning.validation", &state.validation),
        ("action_planning.repositories", &state.repositories),
        ("action_planning.runfiles_nodes", &state.runfiles_nodes),
        (
            "action_planning.target_identities",
            &state.target_identities,
        ),
        ("action_planning.aspect_providers", &state.aspect_providers),
        (
            "action_planning.aspect_target_identities",
            &state.aspect_target_identities,
        ),
        // W1a's memoized dependency views, after the maps whose runfiles
        // and Target values they share (those count there).
        ("action_planning.views", &state.views),
        ("engine.toolchain_contexts", &engine.toolchain_contexts),
        ("engine.exec_group_contexts", &engine.exec_group_contexts),
        ("engine.exec_configurations", &engine.exec_configurations),
        ("engine.exec_transitions", &engine.exec_transitions),
        ("engine.edge_configs", &engine.edge_configs),
        ("engine.rule_delegations", &engine.rule_delegations),
        (
            "engine.group_exec_configurations",
            &engine.group_exec_configurations,
        ),
        ("engine.transition_cache", &engine.transition_cache),
        ("engine.source_targets", &engine.source_targets),
        ("engine.analyzed_sources", &engine.analyzed_sources),
        ("engine.declared_inputs", &engine.declared_inputs),
        ("engine.generated", &engine.generated),
        ("engine.missing_sources", &engine.missing_sources),
        ("engine.source_directories", &engine.source_directories),
        (
            "engine.empty_configuration_rules",
            &engine.empty_configuration_rules,
        ),
        (
            "engine.toolchain_implementations",
            &engine.toolchain_implementations,
        ),
    ]
}

/// The flame-graph root key of the pre-visited frozen heaps (their
/// handles' few inline bytes are not reported).
const KEY_HEAPS: &str = "frozen_heaps";
const KEY_OWNED: Key = Key::new("owned");
const KEY_ARC: Key = Key::new("arc");
const KEY_STARLARK: Key = Key::new("starlark");
/// The bucket of the native walk's estimated malloc overhead.
const MALLOC_OVERHEAD: &str = "native.malloc_overhead_est";
/// An `Arc` allocation's header (strong and weak counts).
const ARC_HEADER: usize = 2 * size_of::<usize>();

/// Run the measurement; returns allocative's flame-graph text
/// (`stack;frames <bytes>` lines) and the estimated malloc overhead of the
/// native walk.
fn measure(heaps: &[FrozenHeapRef], groups: &[Group<'_>]) -> (String, usize) {
    let mut builder = FlameGraphBuilder::default();
    let mut overhead = 0;
    {
        let mut root = builder.root_visitor();
        // Heaps first, dependencies before dependents (DFS postorder):
        // each heap's refs are already counted when it is visited, so
        // allocative's recursion through heap refs stays shallow.
        for heap in heaps {
            root.visit_field(Key::new(KEY_HEAPS), heap);
        }
        for &(name, value) in groups {
            overhead += walk_group(&mut root, name, value);
        }
        root.exit();
    }
    (builder.finish().flamegraph().write(), overhead)
}

/// Walk one bucket; returns its estimated malloc overhead.
fn walk_group(root: &mut Visitor<'_>, name: &'static str, value: &dyn Footprint) -> usize {
    let key = Key::new(name);
    let mut node = root.enter_unique(key.clone(), 0);
    let (bytes, double, overhead) = {
        let mut walker = Walker {
            visitor: &mut node,
            group: key,
            double: 0,
            overhead: 0,
            part: None,
        };
        let bytes = value.heap(&mut walker);
        (bytes, walker.double, walker.overhead)
    };
    node.visit_simple(KEY_OWNED, bytes.saturating_sub(double));
    node.exit();
    overhead
}

type Rows = Vec<(String, u64)>;

/// Sum allocative's flame-graph lines per bucket: a group's own subtree,
/// or `shared:<Type>` for a shared pointee allocative re-rooted at its
/// type; and per `<group>/part:<part>` detail. Largest first, then by name.
fn buckets(text: &str, groups: &[Group<'_>]) -> (Rows, Rows) {
    let names: HashSet<&str> = groups.iter().map(|(name, _)| *name).collect();
    let mut sums: BTreeMap<String, u64> = BTreeMap::new();
    let mut details: BTreeMap<String, u64> = BTreeMap::new();
    for line in text.lines() {
        let Some((stack, bytes)) = line.rsplit_once(' ') else {
            continue;
        };
        let Ok(bytes) = bytes.parse::<u64>() else {
            continue;
        };
        let mut frames = stack.split(';');
        let first = frames.next().unwrap_or_default();
        let bucket = if names.contains(first) {
            if let Some(part) = frames.next().filter(|frame| frame.starts_with("part:")) {
                *details.entry(format!("{first}/{part}")).or_default() += bytes;
            }
            first.to_owned()
        } else if first == KEY_HEAPS {
            continue;
        } else if first == "ArcInner" {
            // The pointee's type; a bare `ArcInner` line is the
            // allocations' reference-count headers.
            format!("shared:{}", frames.next().map_or("arc_headers", short_type))
        } else {
            format!("shared:{}", short_type(first))
        };
        *sums.entry(bucket).or_default() += bytes;
    }
    let mut sums: Rows = sums.into_iter().filter(|(_, b)| *b > 0).collect();
    let mut details: Rows = details.into_iter().filter(|(_, b)| *b > 0).collect();
    sort_rows(&mut sums);
    sort_rows(&mut details);
    (sums, details)
}

/// Largest first, then by name.
fn sort_rows(rows: &mut Rows) {
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
}

/// `a::b::Type<c::D>` -> `Type`.
fn short_type(name: &str) -> &str {
    let base = name.split('<').next().unwrap_or(name);
    base.rsplit("::").next().unwrap_or(base)
}

/// Frozen heaps reachable from the structures: the DFS postorder the
/// measurement visits them in, and the arena bytes of each heap by the
/// first structure that references it (`PERF-ALLOC-HEAP`).
#[derive(Default)]
struct HeapCensus {
    seen: HashSet<FrozenHeapRef>,
    postorder: Vec<FrozenHeapRef>,
    groups: BTreeMap<&'static str, (u64, u64)>,
}

impl HeapCensus {
    fn collect(engine: &Engine, state: &ActionPlanning, plan: &Plan) -> Self {
        let mut census = HeapCensus::default();
        for heap in engine.repositories.build_bzl_cache().frozen_heaps() {
            census.heap("bzl_modules", &heap);
        }
        for package in engine.packages.values() {
            for rule in package.rules.values() {
                census.rule("rules", rule);
            }
        }
        for (_, rule) in plan {
            census.rule("rules", rule);
        }
        for rule in engine.configured_rules.values() {
            census.rule("rules", rule);
        }
        for providers in state.custom_providers.values() {
            census.values("custom_providers", providers.values());
        }
        census.values("native_output_groups", state.native_output_groups.values());
        for providers in state.aspect_providers.values() {
            census.values("aspect_providers", providers.values());
        }
        for target in state.target_identities.values() {
            census.values("target_identities", target.providers.values());
        }
        for target in state.aspect_target_identities.values() {
            census.values("target_identities", target.providers.values());
        }
        census
    }

    fn rule(&mut self, group: &'static str, rule: &Rule) {
        if let Some(custom) = &rule.custom {
            self.values(group, std::iter::once(&custom.implementation));
            self.values(group, custom.subrules.iter());
            self.values(group, custom.top_level_functions.iter());
            self.values(group, custom.transitions.values());
        }
    }

    fn values<'a>(
        &mut self,
        group: &'static str,
        values: impl Iterator<Item = &'a OwnedFrozenValue>,
    ) {
        for value in values {
            self.heap(group, value.owner());
        }
    }

    /// Iterative DFS over heap refs (dependency chains can be deep).
    fn heap(&mut self, group: &'static str, root: &FrozenHeapRef) {
        if !self.seen.insert(root.clone()) {
            return;
        }
        let refs = |heap: &FrozenHeapRef| heap.refs().cloned().collect::<Vec<_>>();
        let mut stack: Vec<(FrozenHeapRef, Vec<FrozenHeapRef>, usize)> =
            vec![(root.clone(), refs(root), 0)];
        while let Some(top) = stack.last_mut() {
            if top.2 < top.1.len() {
                let child = top.1[top.2].clone();
                top.2 += 1;
                if self.seen.insert(child.clone()) {
                    let child_refs = refs(&child);
                    stack.push((child, child_refs, 0));
                }
            } else if let Some((heap, _, _)) = stack.pop() {
                let entry = self.groups.entry(group).or_default();
                entry.0 += heap.allocated_bytes() as u64;
                entry.1 += 1;
                self.postorder.push(heap);
            }
        }
    }
}

/// The walk state of one bucket: allocative's visitor (the shared
/// "already counted" set lives behind it), the bucket's flame-graph key
/// and the inline bytes counted twice (by a container's arithmetic and
/// again by an allocative visit of the same value).
pub(super) struct Walker<'v, 'b> {
    visitor: &'v mut Visitor<'b>,
    group: Key,
    double: usize,
    /// Estimated malloc overhead of the allocations counted ([`malloc_chunk`]).
    overhead: usize,
    /// The `part:` detail being measured ([`Walker::in_part`]): shared
    /// pointees reached inside it are reported under it too.
    part: Option<&'static str>,
}

impl Walker<'_, '_> {
    /// A shared allocation at `ptr`: measured (by `measure`) and counted
    /// under the current bucket the first time any bucket reaches it.
    fn shared(
        &mut self,
        ptr: *const (),
        measure: impl for<'x, 'y> FnOnce(&mut Walker<'x, 'y>) -> usize,
    ) {
        let group = self.group.clone();
        if let Some(mut shared) = self.visitor.enter_shared(KEY_ARC, 0, ptr) {
            let mut node = shared.enter_unique(group.clone(), 0);
            let (bytes, double, overhead) = {
                let mut inner = Walker {
                    visitor: &mut node,
                    group,
                    double: 0,
                    overhead: 0,
                    part: self.part,
                };
                let bytes = measure(&mut inner);
                (bytes, inner.double, inner.overhead)
            };
            self.overhead += overhead;
            let key = self.part.map_or(KEY_OWNED, Key::new);
            node.visit_simple(key, bytes.saturating_sub(double));
            node.exit();
            shared.exit();
        }
    }

    /// A Starlark-side value measured by its own `Allocative` impl (its
    /// inline bytes were already counted by the containing structure).
    fn allocative<T: Allocative>(&mut self, value: &T) -> usize {
        self.double += std::mem::size_of_val(value);
        self.visitor.visit_field(KEY_STARLARK, value);
        0
    }

    /// One heap allocation of `bytes` payload: returns `bytes`, records
    /// its estimated malloc overhead.
    fn alloc(&mut self, bytes: usize) -> usize {
        self.overhead += malloc_chunk(bytes) - bytes;
        bytes
    }

    /// Measure a `part:` detail of the current bucket (reported under
    /// it, still inside the bucket's total; shared pointees reached inside
    /// are reported under the same part).
    fn in_part(&mut self, part: &'static str, measure: impl FnOnce(&mut Self) -> usize) {
        let outer = self.part.replace(part);
        let double_before = self.double;
        let bytes = measure(self);
        let double = self.double - double_before;
        self.double = double_before;
        self.part = outer;
        let bytes = bytes.saturating_sub(double);
        if bytes > 0 {
            self.visitor.visit_simple(Key::new(part), bytes);
        }
    }
}

/// glibc's chunk size for a `bytes` request on 64-bit: 8-byte header,
/// 16-byte granules, 32-byte minimum; page-rounded mmap chunks above the
/// default 128 KiB threshold. 0 for no allocation.
fn malloc_chunk(bytes: usize) -> usize {
    match bytes {
        0 => 0,
        _ if bytes >= 128 * 1024 => (bytes + 16).div_ceil(4096) * 4096,
        _ => (bytes + 8).div_ceil(16).max(2) * 16,
    }
}

/// Heap bytes a value owns beyond its own inline size; shared pointees
/// go through [`Walker::shared`].
pub(super) trait Footprint {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize;
}

/// Bytes of std's B-tree nodes for `len` entries: 11 slots per node,
/// ~8 used on average, internal nodes adding 12 edge pointers.
fn btree_bytes<K, V>(len: usize) -> usize {
    let (nodes, internal) = btree_nodes(len);
    let leaf = 2 * size_of::<usize>() + BTREE_SLOTS * (size_of::<K>() + size_of::<V>());
    nodes * leaf + internal * (BTREE_SLOTS + 1) * size_of::<usize>()
}

const BTREE_SLOTS: usize = 11;

/// (all nodes, internal nodes) of a B-tree of `len` entries.
fn btree_nodes(len: usize) -> (usize, usize) {
    let nodes = len.div_ceil(8);
    (nodes, if nodes > 1 { nodes / 9 } else { 0 })
}

impl Walker<'_, '_> {
    /// A B-tree's node bytes, with each node's malloc overhead recorded.
    fn btree<K, V>(&mut self, len: usize) -> usize {
        let bytes = btree_bytes::<K, V>(len);
        let (nodes, _) = btree_nodes(len);
        if let Some(per_node) = bytes.checked_div(nodes) {
            self.overhead += nodes * (malloc_chunk(per_node) - per_node);
        }
        bytes
    }
}

/// Bytes of a hashbrown table of `capacity` (usable slots) entries `T`:
/// power-of-two buckets at 7/8 load plus one control byte per bucket.
fn hash_table_bytes<T>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = if capacity < 8 {
        (capacity + 1).next_power_of_two()
    } else {
        (capacity * 8 / 7).next_power_of_two()
    };
    buckets * size_of::<T>() + buckets + 16
}

macro_rules! leaf {
    ($($ty:ty),* $(,)?) => {
        $(impl Footprint for $ty {
            fn heap(&self, _: &mut Walker<'_, '_>) -> usize {
                0
            }
        })*
    };
}

/// A struct's owned heap bytes: the sum over the listed fields (fields not
/// listed count with their inline size only).
macro_rules! fields {
    ($ty:ty { $($field:ident),* $(,)? }) => {
        impl Footprint for $ty {
            fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
                0 $(+ self.$field.heap(walker))*
            }
        }
    };
}

/// Plain Rust data (no shared pointers worth following) measured by its
/// `Allocative` derive.
macro_rules! unique_allocative {
    ($($ty:ty),* $(,)?) => {
        $(impl Footprint for $ty {
            fn heap(&self, _: &mut Walker<'_, '_>) -> usize {
                allocative::size_of_unique_allocated_data(self)
            }
        })*
    };
}

/// Starlark-side data (shared depset/runfiles DAGs, frozen heaps) measured
/// through the builder.
macro_rules! starlark_allocative {
    ($($ty:ty),* $(,)?) => {
        $(impl Footprint for $ty {
            fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
                walker.allocative(self)
            }
        })*
    };
}

leaf!(
    bool,
    u8,
    u16,
    u32,
    u64,
    usize,
    i32,
    i64,
    &'static str,
    &'static [u8],
    [u8; 32],
    ConfigId,
    crate::shared_actions::ActionKey,
    // Heaps are counted by the census / pre-visit, never per handle.
    OwnedFrozenValue,
    FrozenHeapRef,
    // Shallow: counted with their inline size where an `Arc` holds them.
    crate::ctx_surface::CtxSurface,
    crate::toolchain_info::ToolchainContextData,
    crate::exec_groups::ExecGroupsData,
    crate::platforms::PlatformInfo,
    crate::toolchains::DeclaredToolchainInfo,
);

impl Footprint for String {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        walker.alloc(self.capacity())
    }
}

impl Footprint for PathBuf {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        walker.alloc(self.capacity())
    }
}

impl Footprint for std::ffi::OsString {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        walker.alloc(self.capacity())
    }
}

impl<T: Footprint> Footprint for Vec<T> {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        walker.alloc(self.capacity() * size_of::<T>())
            + self.iter().map(|item| item.heap(walker)).sum::<usize>()
    }
}

impl<T: Footprint> Footprint for Option<T> {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        self.as_ref().map_or(0, |value| value.heap(walker))
    }
}

impl<T: Footprint> Footprint for Box<T> {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        walker.alloc(size_of::<T>()) + (**self).heap(walker)
    }
}

impl<A: Footprint, B: Footprint> Footprint for (A, B) {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        self.0.heap(walker) + self.1.heap(walker)
    }
}

impl<A: Footprint, B: Footprint, C: Footprint> Footprint for (A, B, C) {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        self.0.heap(walker) + self.1.heap(walker) + self.2.heap(walker)
    }
}

impl<T: Footprint> Footprint for Arc<T> {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        let value: &T = self;
        walker.shared(Arc::as_ptr(self).cast(), |walker| {
            walker.alloc(ARC_HEADER + size_of::<T>()) + value.heap(walker)
        });
        0
    }
}

impl<T: Footprint> Footprint for Arc<[T]> {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        let items: &[T] = self;
        walker.shared(Arc::as_ptr(self).cast(), |walker| {
            walker.alloc(ARC_HEADER + std::mem::size_of_val(items))
                + items.iter().map(|item| item.heap(walker)).sum::<usize>()
        });
        0
    }
}

impl<K: Footprint, V: Footprint> Footprint for BTreeMap<K, V> {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        walker.btree::<K, V>(self.len())
            + self
                .iter()
                .map(|(key, value)| key.heap(walker) + value.heap(walker))
                .sum::<usize>()
    }
}

impl<K: Footprint> Footprint for BTreeSet<K> {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        walker.btree::<K, ()>(self.len()) + self.iter().map(|key| key.heap(walker)).sum::<usize>()
    }
}

impl<K: Footprint, V: Footprint, S> Footprint for HashMap<K, V, S> {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        walker.alloc(hash_table_bytes::<(K, V)>(self.capacity()))
            + self
                .iter()
                .map(|(key, value)| key.heap(walker) + value.heap(walker))
                .sum::<usize>()
    }
}

impl<K: Footprint, S> Footprint for HashSet<K, S> {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        walker.alloc(hash_table_bytes::<K>(self.capacity()))
            + self.iter().map(|key| key.heap(walker)).sum::<usize>()
    }
}

// Labels, keys, files and artifacts.
fields!(Label {
    repository,
    package,
    name
});
fields!(PackageId {
    repository,
    package
});
fields!(ConfiguredTargetKey { label });
fields!(File {
    path,
    short_path,
    owner
});
// W1a: an Artifact caches its File (one Arc per artifact, shared by its
// copies in `dry`, the declared map and the artifact index).
fields!(Artifact {
    real,
    exec,
    producer,
    owner,
    file
});
// One producer per producing action, shared by its outputs' entries.
fields!(OutputProducer {
    owner,
    label,
    action
});
fields!(crate::aspects::AspectApplicationKey {
    base,
    aspect,
    bases
});

// Packages and rules (`Rule` is cloned per configured target into the
// plan and `configured_rules`).
fields!(Package {
    rules,
    exports,
    default_visibility,
    macro_names,
    build_file,
    direct_loads,
    load_graph
});
/// A rule's heap data by part (`PERF-ALLOC-DETAIL`): the per-instance
/// copies W3 targets (schema, attribute values, declaration) apart from
/// the rest. Its inline size is in the containing buffer (see
/// `PERF-ALLOC-SIZES`).
impl Footprint for Rule {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        if let Some(custom) = &self.custom {
            walker.in_part("part:schema", |w| custom.schema.heap(w));
            walker.in_part("part:attrs", |w| custom.attrs.heap(w));
            walker.in_part("part:declaration", |w| custom.declaration.heap(w));
            walker.in_part("part:surface", |w| custom.surface.heap(w));
            walker.in_part("part:toolchains", |w| {
                custom.toolchains.heap(w) + custom.exec_groups.heap(w)
            });
            walker.in_part("part:functions", |w| {
                custom.subrules.heap(w)
                    + custom.top_level_functions.heap(w)
                    + custom.transitions.heap(w)
            });
        }
        walker.in_part("part:native", |w| self.native.heap(w));
        walker.in_part("part:location", |w| {
            self.location.heap(w)
                + self.call_stack.heap(w)
                + self.generator.heap(w)
                + self.macro_owner.heap(w)
        });
        self.custom
            .as_ref()
            .map_or(0, |custom| custom.build_setting.heap(walker))
            + self.label.heap(walker)
            + self.srcs.heap(walker)
            + self.outs.heap(walker)
            + self.cmd.heap(walker)
            + self.visibility.heap(walker)
            + self.unanalyzed.heap(walker)
            + self.package_metadata.heap(walker)
    }
}
fields!(MacroOwner {
    definition_package,
    name
});
fields!(Generator {
    name,
    function,
    location
});
fields!(crate::custom_rule::InvocationDeclaration {
    implicit_outputs,
    unanalyzed,
    transition_gaps,
    toolchain_gaps,
    toolchain_types,
    fragments,
    exec_compatible_with,
    definition_label,
    definition_environment_label,
    configurable,
    attr_order,
    configurable_labels,
    rule_class,
    builtin,
    explicit,
    definition_stack
});
fields!(crate::custom_rule::BuiltinAttributes {
    target,
    visibility,
    deprecation,
    generator
});
fields!(crate::native_rules::NativeTarget {
    rule_class,
    attrs,
    configurable,
    tags,
    input_labels
});
unique_allocative!(
    crate::custom_rule::AttrValue,
    crate::custom_rule::AttrSpec,
    crate::build_settings::BuildSetting,
);

impl Footprint for Visibility {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        match self {
            Visibility::Labels(labels) => labels.heap(walker),
            Visibility::Public | Visibility::Private => 0,
        }
    }
}

impl Footprint for crate::custom_rule::ConfigurablePart {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        match self {
            crate::custom_rule::ConfigurablePart::Plain(value) => value.heap(walker),
            crate::custom_rule::ConfigurablePart::Select {
                branches,
                no_match_error,
            } => branches.heap(walker) + no_match_error.heap(walker),
        }
    }
}

// Configured nodes.
fields!(ConfiguredNode {
    matching,
    actual,
    platform,
    declared_toolchain
});
// Toolchain resolution results kept per configured target.
fields!(crate::toolchains::UnloadedToolchainContext {
    target_platform,
    exec_platform,
    toolchain_types,
    requested,
    resolved
});
fields!(crate::toolchains::ToolchainTypeRequirement { label });
fields!(crate::toolchains::ToolchainTypeInfo {
    type_label,
    no_match_error
});
fields!(crate::exec_groups::TargetExecGroups { groups });
fields!(crate::exec_groups::ResolvedGroup {
    name,
    types,
    context
});
fields!(crate::configured_attributes::ConfigMatching {
    label,
    settings,
    flags,
    constraints
});

// Planned actions. A shared input group (PA-1) counts once, toward the
// first bucket that reaches it (`action_planning.nodes`).
fields!(crate::scheduler::PlannedAction {
    action,
    dependencies,
    groups
});
fields!(crate::scheduler::InputGroup {
    root,
    inputs,
    dependencies,
    tree_inputs,
    constant_metadata,
    empty_files,
    sources
});
fields!(super::SourceInputs { own, groups });
fields!(crate::scheduler::Dependency {
    action_id,
    destination
});
fields!(crate::action::Action {
    id,
    command,
    inputs,
    outputs,
    write,
    program,
    arguments,
    environment,
    execution_info,
    extra,
    embedded_inputs
});
fields!(crate::action::WriteAction { content });
fields!(crate::action::ActionExtra {
    tree_outputs,
    tree_inputs,
    param_files,
    inherited_environment,
    client_environment,
    execution_info,
    mnemonic,
    constant_metadata
});

// Runfiles and Target values (their depset/runfiles DAGs are Starlark
// data, measured through allocative).
fields!(RunfilesNodes {
    tree,
    manifest,
    repo_mapping,
    executable,
    entries,
    conflicts,
    location
});
fields!(crate::runfiles::TargetRunfiles {
    provider,
    files_to_run,
    run
});
fields!(crate::runfiles::RunfilesProvider { default, data });
fields!(crate::runfiles::RunSettings {
    args,
    environment,
    inherited
});
starlark_allocative!(
    crate::runfiles::Runfiles,
    crate::runfiles::FilesToRun,
    crate::starlark_types::Depset,
);

impl Footprint for crate::starlark_types::Target {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        allocative::size_of_unique_allocated_data(&self.label)
            + self.files.heap(walker)
            + self.providers.heap(walker)
            + self.provider_keys.heap(walker)
            + self.runfiles.heap(walker)
            + self.files_depset.heap(walker)
    }
}

// Dependency views (W1a): one shared view per edge key; its runfiles and
// Target are the planned target's own (counted with `runfiles_info` /
// `target_identities`), its files and provider maps are the view's.
impl Footprint for crate::analysis::DependencyTarget {
    fn heap(&self, walker: &mut Walker<'_, '_>) -> usize {
        let view: &crate::analysis::DependencyView = self;
        walker.shared(std::ptr::from_ref(view).cast(), |walker| {
            walker.alloc(ARC_HEADER + size_of::<crate::analysis::DependencyView>())
                + view.heap(walker)
        });
        0
    }
}
fields!(crate::analysis::DependencyView {
    label,
    files,
    providers,
    provider_keys,
    runfiles,
    rule_class,
    identity
});

#[cfg(test)]
mod tests {
    use super::*;

    fn walk_text(groups: &[Group<'_>]) -> String {
        measure(&[], groups).0
    }

    #[test]
    fn shared_pointees_count_once_toward_the_first_bucket() {
        let shared = Arc::new(String::from("0123456789abcdef"));
        let first = vec![shared.clone(), shared.clone()];
        let second = vec![shared.clone()];
        let groups: Vec<Group<'_>> = vec![("first", &first), ("second", &second)];
        let (buckets, _) = buckets(&walk_text(&groups), &groups);
        let get = |name: &str| {
            buckets
                .iter()
                .find(|(bucket, _)| bucket == name)
                .map_or(0, |(_, bytes)| *bytes)
        };
        let pointee = (ARC_HEADER + size_of::<String>() + shared.capacity()) as u64;
        let handles = |n: usize| (n * size_of::<Arc<String>>()) as u64;
        assert_eq!(get("first"), handles(2) + pointee);
        assert_eq!(get("second"), handles(1));
    }

    #[test]
    fn plain_containers_are_sized_by_capacity() {
        let mut names: Vec<String> = Vec::with_capacity(4);
        names.push(String::with_capacity(10));
        names.push(String::with_capacity(20));
        let groups: Vec<Group<'_>> = vec![("names", &names)];
        let (buckets, details) = buckets(&walk_text(&groups), &groups);
        assert_eq!(
            buckets,
            vec![("names".to_owned(), (4 * size_of::<String>() + 30) as u64)]
        );
        assert!(details.is_empty());
        // Overhead: 96-byte buffer -> 112-byte chunk; 10 -> 32; 20 -> 32.
        assert_eq!(measure(&[], &groups).1, 16 + 22 + 12);
    }

    #[test]
    fn malloc_chunks_follow_glibc_rounding() {
        assert_eq!(malloc_chunk(0), 0);
        assert_eq!(malloc_chunk(1), 32);
        assert_eq!(malloc_chunk(24), 32);
        assert_eq!(malloc_chunk(25), 48);
        assert_eq!(malloc_chunk(96), 112);
        assert_eq!(malloc_chunk(128 * 1024), 33 * 4096);
    }

    #[test]
    fn rule_parts_are_details_of_their_bucket() {
        let text = "plan;owned 100\nplan;part:schema 40\nplan;part:attrs 2\npackages;part:schema 7\npackages;arc 0\n";
        let plan: Vec<String> = Vec::new();
        let packages: Vec<String> = Vec::new();
        let groups: Vec<Group<'_>> = vec![("packages", &packages), ("plan", &plan)];
        let (buckets, details) = buckets(text, &groups);
        assert_eq!(
            buckets,
            vec![("plan".to_owned(), 142), ("packages".to_owned(), 7)]
        );
        assert_eq!(
            details,
            vec![
                ("plan/part:schema".to_owned(), 40),
                ("packages/part:schema".to_owned(), 7),
                ("plan/part:attrs".to_owned(), 2),
            ]
        );
    }

    #[test]
    fn btree_and_hash_estimates_grow_with_entries() {
        assert_eq!(btree_bytes::<u64, u64>(0), 0);
        let small = btree_bytes::<u64, u64>(8);
        let large = btree_bytes::<u64, u64>(8_000);
        assert!(small >= 8 * 16, "{small}");
        assert!((8_000 * 16..8_000 * 16 * 2).contains(&large), "{large}");
        assert_eq!(hash_table_bytes::<u64>(0), 0);
        // 3 usable slots: 4 buckets.
        assert_eq!(hash_table_bytes::<u64>(3), 4 * 8 + 4 + 16);
        // 14 usable slots: 16 buckets.
        assert_eq!(hash_table_bytes::<u64>(14), 16 * 8 + 16 + 16);
    }

    #[test]
    fn bucket_names_shorten_shared_types() {
        let text = "a;owned 10\nArcInner 3\nArcInner;starlark::values::layout::heap::heap_type::FrozenFrozenHeap;arena 7\nArcInner;bazel_rs::starlark_types::owned::DepsetNode<x::Y> 5\nother_root;x 1\nfrozen_heaps;Option 8\n";
        let a: Vec<String> = Vec::new();
        let groups: Vec<Group<'_>> = vec![("a", &a)];
        assert_eq!(
            buckets(text, &groups).0,
            vec![
                ("a".to_owned(), 10),
                ("shared:FrozenFrozenHeap".to_owned(), 7),
                ("shared:DepsetNode".to_owned(), 5),
                ("shared:arc_headers".to_owned(), 3),
                ("shared:other_root".to_owned(), 1),
            ]
        );
    }
}
