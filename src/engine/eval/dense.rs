//! Tables keyed by [`NodeId`] as dense vectors (ids are dense: the
//! interner's arena assigns them from 0 in first-sight order). A lookup is
//! one index instead of a hash probe; the linearizer reads these per event.
//!
//! Iteration (where offered) follows the ids, which are not deterministic
//! across thread counts: nothing observable may follow it (as for the hash
//! maps these replace, whose order is arbitrary).

use super::key::NodeId;

/// A map from node ids to `V`: a slot per id up to the largest inserted.
#[derive(Clone, Debug)]
pub(crate) struct DenseMap<V> {
    slots: Vec<Option<V>>,
    len: usize,
}

impl<V> Default for DenseMap<V> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            len: 0,
        }
    }
}

impl<V> DenseMap<V> {
    /// The value of `node`.
    #[inline]
    pub(crate) fn get(&self, node: &NodeId) -> Option<&V> {
        self.slots.get(node.0 as usize)?.as_ref()
    }

    /// The value of `node`, mutably.
    #[inline]
    pub(crate) fn get_mut(&mut self, node: &NodeId) -> Option<&mut V> {
        self.slots.get_mut(node.0 as usize)?.as_mut()
    }

    /// Whether `node` has a value.
    #[inline]
    pub(crate) fn contains_key(&self, node: &NodeId) -> bool {
        self.get(node).is_some()
    }

    fn slot(&mut self, node: NodeId) -> &mut Option<V> {
        let index = node.0 as usize;
        if self.slots.len() <= index {
            // Grow geometrically (ids arrive roughly in order).
            let len = (index + 1).max(self.slots.len() * 2).max(64);
            self.slots.resize_with(len, || None);
        }
        &mut self.slots[index]
    }

    /// Set `node`'s value; the old one.
    pub(crate) fn insert(&mut self, node: NodeId, value: V) -> Option<V> {
        let old = self.slot(node).replace(value);
        self.len += usize::from(old.is_none());
        old
    }

    /// `node`'s value, inserted by `make` if it has none.
    pub(crate) fn get_or_insert_with(&mut self, node: NodeId, make: impl FnOnce() -> V) -> &mut V {
        let index = node.0 as usize;
        if self.slot(node).is_none() {
            self.slots[index] = Some(make());
            self.len += 1;
        }
        self.slots[index].as_mut().expect("present")
    }

    /// Take `node`'s value.
    pub(crate) fn remove(&mut self, node: &NodeId) -> Option<V> {
        let old = self.slots.get_mut(node.0 as usize)?.take();
        self.len -= usize::from(old.is_some());
        old
    }

    /// The number of values.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Whether no node has a value.
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Room for ids below `ids` without growing.
    pub(crate) fn reserve_ids(&mut self, ids: usize) {
        if self.slots.len() < ids {
            self.slots.resize_with(ids, || None);
        }
    }

    /// The values with their ids, by id.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (NodeId, &V)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| Some((NodeId(index as u32), slot.as_ref()?)))
    }
}

impl<V> IntoIterator for DenseMap<V> {
    type Item = (NodeId, V);
    type IntoIter = std::iter::FilterMap<
        std::iter::Enumerate<std::vec::IntoIter<Option<V>>>,
        fn((usize, Option<V>)) -> Option<(NodeId, V)>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        fn pair<V>((index, slot): (usize, Option<V>)) -> Option<(NodeId, V)> {
            Some((NodeId(index as u32), slot?))
        }
        self.slots
            .into_iter()
            .enumerate()
            .filter_map(pair as fn((usize, Option<V>)) -> Option<(NodeId, V)>)
    }
}

impl<V> FromIterator<(NodeId, V)> for DenseMap<V> {
    fn from_iter<I: IntoIterator<Item = (NodeId, V)>>(iter: I) -> Self {
        let mut map = Self::default();
        for (node, value) in iter {
            map.insert(node, value);
        }
        map
    }
}

/// A set of node ids: a bit per id up to the largest inserted.
#[derive(Clone, Debug, Default)]
pub(crate) struct DenseSet {
    words: Vec<u64>,
    len: usize,
}

impl DenseSet {
    /// Whether `node` is in the set.
    #[inline]
    pub(crate) fn contains(&self, node: &NodeId) -> bool {
        let index = node.0 as usize;
        self.words
            .get(index / 64)
            .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    /// Add `node`; whether it was not in the set.
    pub(crate) fn insert(&mut self, node: NodeId) -> bool {
        let index = node.0 as usize;
        if self.words.len() <= index / 64 {
            let len = (index / 64 + 1).max(self.words.len() * 2).max(16);
            self.words.resize(len, 0);
        }
        let word = &mut self.words[index / 64];
        let bit = 1 << (index % 64);
        let new = *word & bit == 0;
        *word |= bit;
        self.len += usize::from(new);
        new
    }

    /// Remove `node`; whether it was in the set.
    pub(crate) fn remove(&mut self, node: &NodeId) -> bool {
        let index = node.0 as usize;
        let Some(word) = self.words.get_mut(index / 64) else {
            return false;
        };
        let bit = 1 << (index % 64);
        let had = *word & bit != 0;
        *word &= !bit;
        self.len -= usize::from(had);
        had
    }

    /// The number of ids in the set.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Whether the set is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_map_behaves_as_a_map() {
        let mut map = DenseMap::default();
        assert!(map.is_empty());
        assert_eq!(map.insert(NodeId(5), "a"), None);
        assert_eq!(map.insert(NodeId(5), "b"), Some("a"));
        assert_eq!(map.insert(NodeId(1000), "c"), None);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&NodeId(5)), Some(&"b"));
        assert_eq!(map.get(&NodeId(6)), None);
        assert_eq!(map.get(&NodeId(100_000)), None);
        assert!(map.contains_key(&NodeId(1000)));
        assert_eq!(*map.get_or_insert_with(NodeId(7), || "d"), "d");
        assert_eq!(*map.get_or_insert_with(NodeId(7), || "e"), "d");
        assert_eq!(map.len(), 3);
        assert_eq!(map.remove(&NodeId(5)), Some("b"));
        assert_eq!(map.remove(&NodeId(5)), None);
        assert_eq!(map.remove(&NodeId(1 << 30)), None);
        assert_eq!(map.len(), 2);
        let pairs: Vec<_> = map.iter().map(|(node, value)| (node.0, *value)).collect();
        assert_eq!(pairs, vec![(7, "d"), (1000, "c")]);
        let collected: DenseMap<&str> = map.into_iter().collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected.get(&NodeId(1000)), Some(&"c"));
    }

    #[test]
    fn dense_set_behaves_as_a_set() {
        let mut set = DenseSet::default();
        assert!(!set.contains(&NodeId(3)));
        assert!(set.insert(NodeId(3)));
        assert!(!set.insert(NodeId(3)));
        assert!(set.insert(NodeId(64 * 40 + 1)));
        assert!(set.contains(&NodeId(3)));
        assert!(set.contains(&NodeId(64 * 40 + 1)));
        assert!(!set.contains(&NodeId(64 * 40)));
        assert_eq!(set.len(), 2);
        assert!(set.remove(&NodeId(3)));
        assert!(!set.remove(&NodeId(3)));
        assert!(!set.remove(&NodeId(1 << 30)));
        assert_eq!(set.len(), 1);
        assert!(!set.is_empty());
    }
}
