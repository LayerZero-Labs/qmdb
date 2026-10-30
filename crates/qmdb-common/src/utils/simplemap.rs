//! Simple hashmap using open addressing with linear probing.
//!
//! Features:
//! - No tombstones: uses re-insertion after removal
//! - Zero-cost hasher via trait (no function pointer overhead)
//! - Auto-resize at 75% load factor
//!
//! TODO: Add salt to prevent Hash DoS (attacker can mine collision addresses and activate them via transfer).

use alloc::vec::Vec;
use core::marker::PhantomData;

// Re-export hasher trait and types from parent module
pub use super::{
    AddressHasher, Hash32Hasher, IdentityHasher, Level8NthHasher, PtrHasher, ShortKeyHasher,
    SlotKeyHasher,
};

const GROWTH_FACTOR: usize = 2;
const LOAD_FACTOR_NUM: usize = 3;
const LOAD_FACTOR_DEN: usize = 4;

/// Slot for SimpleMap.
/// Uses repr(align(8)) to ensure keys are 8-byte aligned for efficient short_key extraction.
#[repr(align(8))]
enum Slot<K, V> {
    Empty,
    Occupied(K, V),
}

impl<K: core::fmt::Debug, V: core::fmt::Debug> core::fmt::Debug for Slot<K, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Slot::Empty => write!(f, "Empty"),
            Slot::Occupied(k, v) => f.debug_tuple("Occupied").field(k).field(v).finish(),
        }
    }
}

/// Simple hashmap with open addressing and linear probing.
///
/// - `H`: Hasher type implementing `ShortKeyHasher<K>` (zero-sized, no storage overhead)
/// - Auto-resizes at 75% load factor
pub struct SimpleMap<K, V, H: ShortKeyHasher<K>> {
    slots: Vec<Slot<K, V>>,
    mask: usize,
    len: usize,
    _hasher: PhantomData<H>,
}

impl<K: core::fmt::Debug, V: core::fmt::Debug, H: ShortKeyHasher<K>> core::fmt::Debug
    for SimpleMap<K, V, H>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SimpleMap")
            .field("slots", &self.slots)
            .field("mask", &self.mask)
            .field("len", &self.len)
            .finish()
    }
}

impl<K, V, H> SimpleMap<K, V, H>
where
    H: ShortKeyHasher<K>,
    K: Eq,
{
    /// Creates a new map with specified capacity (minimum 8, rounded to power of 2).
    #[inline]
    pub fn with_capacity(cap: usize) -> Self {
        let cap = cap.max(8).next_power_of_two();
        Self {
            slots: (0..cap).map(|_| Slot::Empty).collect(),
            mask: cap - 1,
            len: 0,
            _hasher: PhantomData,
        }
    }

    /// Computes slot index from hasher result using bitwise AND (faster than modulo).
    #[inline(always)]
    fn compute_idx(&self, hash: u64) -> usize {
        hash as usize & self.mask
    }

    #[inline(always)]
    fn next_idx(&self, idx: usize) -> usize {
        (idx + 1) & self.mask
    }

    /// Finds slot for key. Returns (index, found).
    #[inline(always)]
    fn find_slot(&self, key: &K) -> (usize, bool) {
        let start = self.compute_idx(H::hash(key));
        let mut idx = start;
        loop {
            match &self.slots[idx] {
                Slot::Empty => return (idx, false),
                Slot::Occupied(k, _) if *k == *key => return (idx, true),
                _ => {}
            }
            idx = self.next_idx(idx);
            if idx == start {
                return (idx, false);
            }
        }
    }

    /// Inserts key-value pair, returns old value if key existed.
    #[inline]
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        if self.len * LOAD_FACTOR_DEN > self.slots.len() * LOAD_FACTOR_NUM {
            self.resize();
        }

        let (idx, found) = self.find_slot(&key);
        if found {
            match &mut self.slots[idx] {
                Slot::Occupied(_, v) => Some(core::mem::replace(v, value)),
                _ => unreachable!(),
            }
        } else {
            self.slots[idx] = Slot::Occupied(key, value);
            self.len += 1;
            None
        }
    }

    fn resize(&mut self) {
        let new_cap = self.slots.len() * GROWTH_FACTOR;
        let old_slots =
            core::mem::replace(&mut self.slots, (0..new_cap).map(|_| Slot::Empty).collect());
        self.mask = new_cap - 1;
        self.len = 0;
        for slot in old_slots {
            if let Slot::Occupied(k, v) = slot {
                self.insert(k, v);
            }
        }
    }

    /// Gets value reference for key.
    #[inline]
    pub fn get(&self, key: &K) -> Option<&V> {
        let (idx, found) = self.find_slot(key);
        if found {
            match &self.slots[idx] {
                Slot::Occupied(_, v) => Some(v),
                _ => unreachable!(),
            }
        } else {
            None
        }
    }

    /// Gets mutable value reference for key.
    #[inline]
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let (idx, found) = self.find_slot(key);
        if found {
            match &mut self.slots[idx] {
                Slot::Occupied(_, v) => Some(v),
                _ => unreachable!(),
            }
        } else {
            None
        }
    }

    /// Returns entry count.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Removes key, returns value if existed.
    /// Uses re-insertion to maintain probe chains.
    #[inline]
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let (idx, found) = self.find_slot(key);
        if !found {
            return None;
        }

        let ret = match core::mem::replace(&mut self.slots[idx], Slot::Empty) {
            Slot::Occupied(_, v) => v,
            _ => unreachable!(),
        };
        self.len -= 1;

        // Re-insert subsequent entries to maintain probe chains
        let mut probe = self.next_idx(idx);
        while let Slot::Occupied(_, _) = &self.slots[probe] {
            if let Slot::Occupied(k, v) = core::mem::replace(&mut self.slots[probe], Slot::Empty) {
                self.len -= 1;
                self.insert(k, v);
            }
            probe = self.next_idx(probe);
        }

        Some(ret)
    }

    /// Returns true if empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Iterator over (key, value) pairs.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.slots.iter().filter_map(|s| match s {
            Slot::Occupied(k, v) => Some((k, v)),
            _ => None,
        })
    }

    /// Iterator over keys.
    #[inline]
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(k, _)| k)
    }

    /// Gets or inserts value, returns mutable reference.
    #[inline]
    pub fn entry_or_insert_with<F: FnOnce() -> V>(&mut self, key: K, f: F) -> &mut V {
        if self.len * LOAD_FACTOR_DEN > self.slots.len() * LOAD_FACTOR_NUM {
            self.resize();
        }

        let (idx, found) = self.find_slot(&key);
        if !found {
            self.slots[idx] = Slot::Occupied(key, f());
            self.len += 1;
        }

        match &mut self.slots[idx] {
            Slot::Occupied(_, v) => v,
            _ => unreachable!(),
        }
    }
}

pub struct SimpleMapIter<'a, K, V> {
    inner: core::slice::Iter<'a, Slot<K, V>>,
}

impl<'a, K, V> Iterator for SimpleMapIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.inner.next() {
                Some(Slot::Occupied(k, v)) => return Some((k, v)),
                Some(_) => continue,
                None => return None,
            }
        }
    }
}

impl<'a, K, V, H> IntoIterator for &'a SimpleMap<K, V, H>
where
    H: ShortKeyHasher<K>,
{
    type Item = (&'a K, &'a V);
    type IntoIter = SimpleMapIter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        SimpleMapIter {
            inner: self.slots.iter(),
        }
    }
}

impl<K, V, H1, H2> PartialEq<SimpleMap<K, V, H2>> for SimpleMap<K, V, H1>
where
    K: Eq,
    V: PartialEq,
    H1: ShortKeyHasher<K>,
    H2: ShortKeyHasher<K>,
{
    fn eq(&self, other: &SimpleMap<K, V, H2>) -> bool {
        if self.len != other.len {
            return false;
        }
        self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_operations() {
        let mut map: SimpleMap<u64, u64, IdentityHasher> = SimpleMap::with_capacity(10);

        assert_eq!(map.insert(42, 100), None);
        assert_eq!(map.insert(42, 200), Some(100));
        assert_eq!(map.get(&42), Some(&200));
        assert_eq!(map.len(), 1);

        map.insert(123, 300);
        map.insert(456, 400);
        assert_eq!(map.len(), 3);

        assert_eq!(map.remove(&123), Some(300));
        assert_eq!(map.get(&123), None);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn test_resize() {
        let mut map: SimpleMap<u64, u64, IdentityHasher> = SimpleMap::with_capacity(8);
        let init_cap = map.slots.len();

        for i in 0..20 {
            map.insert(i, i * 10);
        }

        assert!(map.slots.len() > init_cap);
        for i in 0..20 {
            assert_eq!(map.get(&i), Some(&(i * 10)));
        }
    }

    #[test]
    fn test_remove_reinsert() {
        let mut map: SimpleMap<u64, u64, IdentityHasher> = SimpleMap::with_capacity(16);

        for i in 0..10 {
            map.insert(i, i * 10);
        }

        map.remove(&3);
        map.remove(&5);
        map.insert(3, 300);

        assert_eq!(map.get(&3), Some(&300));
        assert_eq!(map.get(&5), None);
    }

    #[test]
    fn test_probe_chain_after_remove() {
        let mut map: SimpleMap<u64, u64, IdentityHasher> = SimpleMap::with_capacity(16);

        for i in 0..8 {
            map.insert(i * 16, i);
        }

        map.remove(&(2 * 16));
        map.remove(&(4 * 16));

        assert_eq!(map.get(&(0 * 16)), Some(&0));
        assert_eq!(map.get(&(1 * 16)), Some(&1));
        assert_eq!(map.get(&(2 * 16)), None);
        assert_eq!(map.get(&(3 * 16)), Some(&3));
        assert_eq!(map.get(&(4 * 16)), None);
        assert_eq!(map.get(&(5 * 16)), Some(&5));
    }
}
