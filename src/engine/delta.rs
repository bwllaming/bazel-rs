//! S1 (docs/PARALLEL_ANALYSIS.md, "S1 — commit deltas"): a Starlark
//! target's commit built as data on the analysis worker that computed it
//! ([`Engine::build_delta`]) and merged by the coordinator in plan order
//! ([`Engine::merge_delta`]). The sequential engine runs the same two
//! functions back to back on one thread.
//!
//! What a delta reads is shared with the workers: the build's artifact
//! index ([`super::ConcurrentArtifactIndex`], read with the target's own
//! plan index), the memoized tool runfiles groups and shared input runs
//! ([`SingleFlight`]), the client environment. What it writes is a list of
//! operations in the order the sequential commit made them; the producers
//! of output paths stay coordinator-only and are written only by the
//! merge.

use super::*;

/// The shards of a [`SingleFlight`] memo (a power of two).
const MEMO_SHARDS: usize = 16;

/// One memo entry: filled once (the fill holds the entry's own lock, so a
/// second filler waits and takes the first one's value).
type Slot<V> = Arc<std::sync::Mutex<Option<Arc<V>>>>;

/// A memo filled once per key by whichever thread needs it first — the
/// coordinator or an analysis worker building a delta — and shared by
/// every later use (one value, so one `Arc` identity, per key). A fill that
/// fails (`None` or an error) leaves the entry empty for the next use.
///
/// A fill only looks artifacts up in the build's index (a workspace status
/// file, which would be registered on first need, makes a worker's delta
/// fall back to the coordinator), so it has no effect a merge must replay:
/// each delta lists the memo keys it used, in order ([`MemoUse`]), for
/// BAZEL_RS_VERIFY_DELTA's comparison.
pub(super) struct SingleFlight<K, V> {
    shards: Box<[std::sync::Mutex<MemoShard<K, V>>]>,
}

/// One shard of a [`SingleFlight`] memo.
type MemoShard<K, V> = HashMap<K, Slot<V>>;

impl<K, V> Default for SingleFlight<K, V> {
    fn default() -> Self {
        Self {
            shards: (0..MEMO_SHARDS).map(|_| Default::default()).collect(),
        }
    }
}

impl<K: std::hash::Hash + Eq + Clone, V> SingleFlight<K, V> {
    fn shard(&self, key: &K) -> &std::sync::Mutex<MemoShard<K, V>> {
        use std::hash::BuildHasher;
        let hash = crate::fast_hash::FxBuildHasher::default().hash_one(key);
        &self.shards[(hash >> 60) as usize & (MEMO_SHARDS - 1)]
    }

    fn slot(&self, key: &K) -> Slot<V> {
        let mut shard = self.shard(key).lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(shard.entry(key.clone()).or_default())
    }

    /// The filled value of `key`, if any.
    pub(super) fn get(&self, key: &K) -> Option<Arc<V>> {
        let slot = {
            let shard = self.shard(key).lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(shard.get(key)?)
        };
        let value = slot.lock().unwrap_or_else(|e| e.into_inner());
        value.clone()
    }

    /// The value of `key`, filled by `fill` unless it is already (`None`
    /// when `fill` returns none: the entry stays empty).
    pub(super) fn get_or_fill<E>(
        &self,
        key: &K,
        fill: impl FnOnce() -> Result<Option<V>, E>,
    ) -> Result<Option<Arc<V>>, E> {
        let slot = self.slot(key);
        let mut value = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(filled) = value.as_ref() {
            return Ok(Some(Arc::clone(filled)));
        }
        Ok(fill()?.map(|filled| {
            let filled = Arc::new(filled);
            *value = Some(Arc::clone(&filled));
            filled
        }))
    }

    /// Every filled value (reports only; no order is observed).
    pub(super) fn values(&self) -> Vec<Arc<V>> {
        let mut values = Vec::new();
        for shard in self.shards.iter() {
            let shard = shard.lock().unwrap_or_else(|e| e.into_inner());
            for slot in shard.values() {
                if let Some(value) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    values.push(Arc::clone(value));
                }
            }
        }
        values
    }
}

/// The memoized tool runfiles groups of a build, by tree identity (PA-1,
/// ActionPlanning::input_groups).
pub(super) type InputGroups = SingleFlight<u64, ExpandedTree>;

/// The shared input runs of a build's memoized depset walks, by the walk's
/// address ([`SharedInputs`], ActionPlanning::input_memos).
pub(super) type InputMemos = SingleFlight<usize, SharedInputs>;
