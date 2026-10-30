//! In-memory index tracking bottom level branch nodes. This is an immutable data structure,
//! which is cheaply cloneable in O(1) and performs COW operations.

use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

use imbl::OrdMap;

use super::Key;
use crate::beatree::branch::BranchNode;

#[derive(Default, Clone)]
pub struct Index {
    first_key_map: OrdMap<Key, Arc<BranchNode>>,
}

impl Index {
    /// Look up the branch that would store the given key.
    ///
    /// This is either a branch whose separator is exactly equal to this key or the branch with the
    /// highest separator less than the key.
    pub fn lookup(&self, key: Key) -> Option<(Key, Arc<BranchNode>)> {
        self.first_key_map
            .get_prev(&key)
            .map(|(sep, b)| (sep.clone(), b.clone()))
    }

    /// Get the first separator greater than the given key.
    pub fn next_key(&self, key: Key) -> Option<Key> {
        self.first_key_map
            .range(RangeFromExclusive { start: key })
            .next()
            .map(|(k, _)| *k)
    }

    /// Remove the branch with the given separator key.
    pub fn remove(&mut self, separator: &Key) -> Option<Arc<BranchNode>> {
        self.first_key_map.remove(separator)
    }

    /// Insert a branch with the given separator key.
    pub fn insert(&mut self, separator: Key, branch: Arc<BranchNode>) -> Option<Arc<BranchNode>> {
        self.first_key_map.insert(separator, branch)
    }

    #[cfg(test)]
    pub fn into_iter(self) -> impl Iterator<Item = (Key, Arc<BranchNode>)> {
        self.first_key_map.into_iter()
    }
}

struct RangeFromExclusive {
    start: Key,
}

impl RangeBounds<Key> for RangeFromExclusive {
    fn start_bound(&self) -> Bound<&Key> {
        Bound::Excluded(&self.start)
    }

    fn end_bound(&self) -> Bound<&Key> {
        Bound::Unbounded
    }

    fn contains<U>(&self, item: &U) -> bool
    where
        U: PartialOrd<Key> + ?Sized,
    {
        item > &self.start
    }
}
