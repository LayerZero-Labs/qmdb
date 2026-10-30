use super::hybrid::ref_unit::RefUnit;
use super::kvlist::{self, KVCursor};
use aes_gcm::Aes256Gcm;
use byteorder::{BigEndian, ByteOrder};
use hex;
use parking_lot::RwLock;
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE, SHARD_COUNT, SHARD_DIV};
use qmdb_common::indexer::indexer_trait::IndexerTrait;
use qmdb_common::indexer::utils::poslist::PosList;
use std::collections::{BTreeSet, HashSet};
use std::ops::Bound::{self, Excluded, Included};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time;

// Unit follows LSM-tree style because:
// Key LSM-tree Characteristics Present:
//     - Write Path - Mutable Buffer
//     - New writes go to small_set (BTreeSet) → acts like memtable
//     - Fast in-memory insertions O(log n)
// Immutable Sorted Storage
//     - big_list stores sorted data → acts like SSTable
//     - Rarely modified after creation
// Lazy Deletion with Tombstones
//     - Deletes mark entries with NIL_MARKER → tombstone marker
//     - Doesn't immediately remove data
// Background Compaction
//     - Asynchronous compact() merges small_set into big_list
//     - Removes tombstones and garbage
//     - Triggered when (num_of_nil + small_set.len()) * 30 > size
// Read Amplification
//     - Reads must check both big_list AND small_set
//     - Must skip tombstones during iteration
//     - Classic LSM trade-off: fast writes, slower reads
// Write Optimization
//     - Append-friendly operations
//     - Minimizes expensive data movement
//     - Batches reorganization during compaction
// In summary: Write-optimized structure with deferred merge operations and tombstone-based deletion —— textbook LSM-tree design pattern.
//
const COMPACT_RATIO: usize = 30;
const COMPACT_TRY_RANGE: usize = 2000; //2000 or 20(debug)

type KV = (u64, [u8; 6]); // 8-byte key -> 6-byte pos
type KVList = kvlist::KVList<9, 10, 6>; // 4-byte key -> 6-byte pos

pub struct Unit {
    size: usize,
    num_of_nil: usize,
    big_list: KVList,
    small_set: BTreeSet<KV>,
}

// This special value is used to mark a KV pair in big_list as nillified.
// Please note it is still a valid value to be stored in small_set BTreeSet.
// Only in big_list it is used as a marker.
static NIL_MARKER: &[u8; 6] = &[255, 255, 255, 255, 255, 255];

impl Unit {
    pub fn len(&self) -> usize {
        self.size
    }

    fn merge_kv(k: u64, v6: &[u8; 6]) -> [u8; 10] {
        let mut elem = [0u8; 10];
        BigEndian::write_u32(&mut elem[..4], (k >> 32) as u32);
        elem[4..].copy_from_slice(&v6[..]);
        elem
    }

    fn split_kv(elem: &[u8; 10]) -> (u64, [u8; 6]) {
        let mut v6 = [0u8; 6];
        v6[..].copy_from_slice(&elem[4..]);
        // only the high 32 bits of k_hi32 is meaningful and low 32 bits are zero
        let k_hi32 = (BigEndian::read_u32(&elem[..4]) as u64) << 32;
        (k_hi32, v6)
    }

    fn to_k10(k: u64) -> [u8; 10] {
        let k8 = k.to_be_bytes();
        let mut k10 = [0u8; 10];
        k10[..4].copy_from_slice(&k8[..4]); //big_list's 4-byte key
        k10
    }

    fn to_v6(v: i64) -> [u8; 6] {
        let v8 = v.to_be_bytes();
        let mut v6 = [0u8; 6];
        v6[..].copy_from_slice(&v8[2..]);
        v6
    }

    fn v6_to_i64(v6: &[u8; 6]) -> i64 {
        let mut bz = [0u8; 8];
        bz[2..].copy_from_slice(&v6[..]);
        i64::from_be_bytes(bz)
    }

    fn append(&mut self, offsets_idx: &mut usize, last_k: &mut u64, k: u64, v6: &[u8; 6]) {
        self.size += 1;
        // When the current KV pair and the previous pair falls into the same
        // hash bucket, we store the current pair in small_set
        let same_bucket = (*last_k >> 32) == (k >> 32) && (k & 0xFFFFFFFF) != 0;
        *last_k = k;
        if v6 == NIL_MARKER || same_bucket {
            // NIL_MARKER cannot be stored in big_list
            self.small_set.insert((k, *v6));
            return;
        }
        let elem = Self::merge_kv(k, v6);
        self.big_list.append(offsets_idx, &elem);
    }

    fn dump_to(&self, out: &mut Self) {
        let mut offsets_idx = usize::MAX;
        let mut last_k = u64::MAX;
        let mut iter = self.small_set.iter();
        let mut kv = iter.next();
        let mut cursor = self.big_list.get_first_cursor();
        while cursor.valid() {
            let elem = self.big_list.at(&cursor);
            let (k_hi32, file_pos) = Self::split_kv(&elem);
            if &file_pos == NIL_MARKER {
                self.big_list.move_forward(&mut cursor);
                continue; // nillified pairs in big_list are ignored
            }
            while kv.is_some() && kv.unwrap().0 < k_hi32 {
                out.append(&mut offsets_idx, &mut last_k, kv.unwrap().0, &kv.unwrap().1);
                kv = iter.next();
            }
            out.append(&mut offsets_idx, &mut last_k, k_hi32, &file_pos);
            self.big_list.move_forward(&mut cursor);
        }
        while kv.is_some() {
            out.append(&mut offsets_idx, &mut last_k, kv.unwrap().0, &kv.unwrap().1);
            kv = iter.next();
        }
    }

    fn find_kv(&self, k: u64, v6: &[u8; 6]) -> (KVCursor, bool) {
        let mut k10 = Self::to_k10(k);
        let mut cursor = self.big_list.find(&k10);
        k10[4..].copy_from_slice(&v6[..]);
        while cursor.valid() {
            let elem = self.big_list.at(&cursor);
            let (k_hi32, _) = Self::split_kv(&elem);
            if (k_hi32 >> 32) != (k >> 32) {
                break;
            }
            if elem == k10 {
                return (cursor, true);
            }
            self.big_list.move_forward(&mut cursor);
        }
        (cursor, false)
    }

    fn _for_each_value<F>(&self, k: u64, mut access: F) -> (KVCursor, bool)
    where
        F: FnMut(i64) -> bool,
    {
        let k10 = Self::to_k10(k);
        // First we iterate big_list
        let found_cursor = self.big_list.find(&k10);
        let mut cursor = found_cursor.clone();
        while cursor.valid() {
            let elem = self.big_list.at(&cursor);
            let (k_hi32, file_pos) = Self::split_kv(&elem);
            if (k_hi32 >> 32) != (k >> 32) {
                break;
            }
            if &file_pos == NIL_MARKER {
                self.big_list.move_forward(&mut cursor);
                continue; // nillified pairs in big_list are ignored
            }
            if access(Self::v6_to_i64(&file_pos)) {
                return (found_cursor, true);
            }
            self.big_list.move_forward(&mut cursor);
        }

        // Then we iterate small_set BTreeSet
        let start = (k, [0u8; 6]);
        let end = (k, [0xFFu8; 6]);
        let range = (Included(&start), Included(&end));
        for &v in self
            .small_set
            .range::<KV, (Bound<&KV>, Bound<&KV>)>(range)
            .rev()
        {
            let file_pos = Self::v6_to_i64(&v.1);
            if access(file_pos) {
                return (found_cursor, true);
            }
        }
        (found_cursor, false)
    }

    fn debug_get_values(&self, k_in: u64) -> HashSet<i64> {
        let mut res = HashSet::new();
        self.for_each_value(k_in, |v| -> bool {
            res.insert(v);
            false // do not exit loop
        });
        res
    }

    fn debug_get_adjacent(&self, k80: &[u8; 10], k_in: u64) -> HashSet<i64> {
        let mut res = HashSet::new();
        self.for_each_adjacent_value(*k80, k_in, |v| -> bool {
            res.insert(v);
            false // do not exit loop
        });
        res
    }
}

pub trait UnitTrait: Send + Sync {
    fn new() -> Self;
    fn add_kv(&mut self, k: u64, v: i64);
    fn erase_kv(&mut self, k: u64, v: i64);
    fn change_kv(&mut self, k: u64, v_old: i64, v_new: i64);
    fn for_each_value<F>(&self, k: u64, access: F)
    where
        F: FnMut(i64) -> bool;
    fn for_each_adjacent_value<F>(&self, k80: [u8; 10], k: u64, access: F)
    where
        F: FnMut(i64) -> bool;
    fn is_compactible(&self) -> bool;
    fn compact(&mut self);
}

impl UnitTrait for Unit {
    fn new() -> Self {
        Self {
            size: 0,
            num_of_nil: 0,
            big_list: KVList::new(512),
            small_set: BTreeSet::new(),
        }
    }

    fn add_kv(&mut self, k: u64, v: i64) {
        let v6 = Self::to_v6(v);
        let success = self.small_set.insert((k, v6));
        assert!(
            success,
            "Add Duplicated KV k={:#016x} v={}",
            k,
            hex::encode(v6)
        );
        self.size += 1;
    }

    fn erase_kv(&mut self, k: u64, v: i64) {
        self.size -= 1;
        let v6 = Self::to_v6(v);
        let (cursor, found_it) = self.find_kv(k, &v6);
        if found_it {
            self.num_of_nil += 1;
            self.big_list.change(&cursor, NIL_MARKER);
            return;
        }
        let removed = self.small_set.remove(&(k, v6));
        assert!(
            removed,
            "Cannot Erase Non-existent KV pair k={:#016x} v={}",
            k,
            hex::encode(v6)
        );
    }

    fn change_kv(&mut self, k: u64, v_old: i64, v_new: i64) {
        let v6 = Self::to_v6(v_old);
        let v6_new = Self::to_v6(v_new);
        let (cursor, found_it) = self.find_kv(k, &v6);
        if found_it {
            //NIL_MARKER is just an nullification marker in big_list
            if &v6_new == NIL_MARKER {
                self.small_set.insert((k, v6_new));
                self.num_of_nil += 1;
            }
            self.big_list.change(&cursor, &v6_new);
            return;
        }
        let removed = self.small_set.remove(&(k, v6));
        assert!(
            removed,
            "Cannot change non-existant KV pair k={:#016x} v={}",
            k,
            hex::encode(v6)
        );
        self.small_set.insert((k, v6_new));
    }

    fn is_compactible(&self) -> bool {
        (self.num_of_nil + self.small_set.len()) * COMPACT_RATIO > self.size
    }

    fn compact(&mut self) {
        let mut out = Self {
            size: 0,
            num_of_nil: 0,
            small_set: BTreeSet::new(),
            big_list: KVList::new(self.size),
        };
        self.dump_to(&mut out);
        assert!(
            self.size == out.size,
            "Compacted size mismatch! {} != {}",
            self.size,
            out.size
        );
        std::mem::swap(self, &mut out);
    }

    fn for_each_value<F>(&self, k: u64, access: F)
    where
        F: FnMut(i64) -> bool,
    {
        self._for_each_value(k, access);
    }

    fn for_each_adjacent_value<F>(&self, mut k80: [u8; 10], k: u64, mut access: F)
    where
        F: FnMut(i64) -> bool,
    {
        // 1) iterate for the current key
        let (mut cursor, done) = self._for_each_value(k, &mut access);
        if done {
            return;
        }

        // 2.1) get the 'prev' from small_set
        let end = (k, [0u8; 6]);
        let start = (0u64, [0u8; 6]);
        let range = (Included(&start), Excluded(&end));
        let small_set = &self.small_set;
        let small_prev = small_set
            .range::<KV, (Bound<&KV>, Bound<&KV>)>(range)
            .next_back();

        // 2.2) get the 'prev' from big_list
        if cursor.valid() {
            self.big_list.move_backward(&mut cursor);
        } else {
            cursor = self.big_list.get_last_cursor();
        }
        while cursor.valid() {
            let elem = self.big_list.at(&cursor);
            let (_, file_pos) = Self::split_kv(&elem);
            if &file_pos != NIL_MARKER {
                break;
            }
            self.big_list.move_backward(&mut cursor);
        }
        let (big_prev, got_big) = if cursor.valid() {
            let elem = self.big_list.at(&cursor);
            let (k_hi32, _) = Self::split_kv(&elem);
            (k_hi32, true)
        } else {
            (0u64, false)
        };

        // 3.1) if the 'prev' from big_list is more adjacent
        if got_big && (small_prev.is_none() || (&small_prev.unwrap().0 >> 32) <= (big_prev >> 32)) {
            while cursor.valid() {
                let elem = self.big_list.at(&cursor);
                let (k_hi32, file_pos) = Self::split_kv(&elem);
                if (k_hi32 >> 32) != (big_prev >> 32) {
                    break;
                }
                BigEndian::write_u64(&mut k80[2..], k_hi32);
                if &file_pos != NIL_MARKER {
                    let file_pos = Self::v6_to_i64(&file_pos);
                    if access(file_pos) {
                        return;
                    }
                }
                self.big_list.move_backward(&mut cursor);
            }
        }

        // 3.2) if the 'prev' from small_set is more adjacent
        if let Some(prev_elem) = small_prev {
            if got_big && (prev_elem.0 >> 32) < (big_prev >> 32) {
                return;
            }
            for &elem in small_set.range::<KV, (Bound<&KV>, Bound<&KV>)>(range).rev() {
                if elem.0 == prev_elem.0 {
                    BigEndian::write_u64(&mut k80[2..], elem.0);
                    let file_pos = Self::v6_to_i64(&elem.1);
                    if access(file_pos) {
                        return;
                    }
                } else {
                    break;
                }
            }
        }
    }
}

// only used in fuzz test
pub struct Unit4Test {
    pub u: Unit,
    pub ref_u: RefUnit,
}

impl Unit4Test {
    pub fn debug_get_kv(&self, k_in: u64) -> (u64, i64) {
        let (k, v) = self.ref_u.debug_get_kv(0, k_in);
        (k, v)
    }
}

impl UnitTrait for Unit4Test {
    fn new() -> Self {
        Self {
            u: Unit::new(),
            ref_u: RefUnit::new(),
        }
    }

    fn add_kv(&mut self, k: u64, v: i64) {
        self.u.add_kv(k, v);
        self.ref_u.add_kv(0, k, v);
    }

    fn erase_kv(&mut self, k: u64, v: i64) {
        self.u.erase_kv(k, v);
        self.ref_u.erase_kv(0, k, v);
    }

    fn change_kv(&mut self, k: u64, v_old: i64, v_new: i64) {
        self.u.change_kv(k, v_old, v_new);
        self.ref_u.change_kv(0, k, v_old, v_new);
    }

    fn for_each_value<F>(&self, k: u64, access: F)
    where
        F: FnMut(i64) -> bool,
    {
        let r = self.ref_u.debug_get_values(0, k);
        let i = self.u.debug_get_values(k);
        if r != i {
            for e in r.iter() {
                if !i.contains(e) {
                    panic!("Fatal for_each_adjacent_value mismatch");
                }
            }
        }
        self.u.for_each_value(k, access);
    }

    fn for_each_adjacent_value<F>(&self, k80: [u8; 10], k: u64, access: F)
    where
        F: FnMut(i64) -> bool,
    {
        let r = self.ref_u.debug_get_adjacent(&k80, 0, k);
        let i = self.u.debug_get_adjacent(&k80, k);
        if r != i {
            for e in r.iter() {
                if !i.contains(e) {
                    panic!("FATAL for_each_adjacent_value mismatch");
                }
            }
        }
        self.u.for_each_adjacent_value(k80, k, access);
    }

    fn is_compactible(&self) -> bool {
        self.u.is_compactible()
    }

    fn compact(&mut self) {
        self.u.compact();
    }
}

pub struct InMemIndexerGeneric<U: UnitTrait> {
    units: Vec<RwLock<U>>,
    sizes: [AtomicUsize; SHARD_COUNT],
}

/// Type alias for the standard in-memory indexer implementation.
pub type InMemIndexer = InMemIndexerGeneric<Unit>;

/// Type alias for the test-specific in-memory indexer implementation.
pub type InMemIndexer4Test = InMemIndexerGeneric<Unit4Test>;

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicUsize = AtomicUsize::new(0);

impl<U: UnitTrait + 'static> InMemIndexerGeneric<U> {
    pub fn with_dir(_dir: String) -> Self {
        Self::new(1 << 16)
    }

    pub fn with_dir_and_cipher(_dir: String, _cipher: Arc<Option<Aes256Gcm>>) -> Self {
        Self::new(1 << 16)
    }

    pub fn new(n: usize) -> Self {
        let mut res = Self {
            units: Vec::with_capacity(n),
            sizes: [ZERO; SHARD_COUNT],
        };
        for _ in 0..n {
            res.units.push(RwLock::new(U::new()));
        }
        res
    }

    fn shrink_value(v: i64) -> i64 {
        assert!(v % 8 == 0, "value not 8x");
        v / 8
    }

    fn scale_value(v: i64) -> i64 {
        v * 8
    }

    fn get_inputs(&self, k80: &[u8], v: i64) -> (usize, u64, i64) {
        let idx = BigEndian::read_u16(&k80[..2]) as usize;
        let k = BigEndian::read_u64(&k80[2..]);
        (idx % self.units.len(), k, Self::shrink_value(v))
    }

    pub fn start_compacting(indexer: Arc<Self>) {
        let len = indexer.units.len();
        Self::_start_compacting(indexer.clone(), 0, len);
    }

    fn _start_compacting(indexer: Arc<Self>, start: usize, end: usize) {
        thread::spawn(move || loop {
            let mut did_nothing_count = 0usize;
            for i in start..end {
                if did_nothing_count > COMPACT_TRY_RANGE {
                    did_nothing_count = 0;
                    thread::sleep(time::Duration::from_millis(100));
                }
                if !indexer.units[i].read().is_compactible() {
                    did_nothing_count += 1;
                    continue;
                }
                let mut unit = indexer.units[i].write();
                unit.compact();
            }
        });
    }
}

impl<U: UnitTrait + 'static> IndexerTrait for InMemIndexerGeneric<U> {
    fn len(&self, shard_id: usize) -> usize {
        // Approximate stats only; no synchronization semantics required.
        self.sizes[shard_id].load(Ordering::Relaxed)
    }

    fn add_kv(&self, k80: &[u8], v: i64, _sn: u64) {
        let (idx, k, v) = self.get_inputs(k80, v);
        let unit = &mut self.units[idx].write();
        unit.add_kv(k, v);
        self.sizes[idx / SHARD_DIV].fetch_add(1, Ordering::Relaxed);
    }

    fn erase_kv(&self, k80: &[u8], v: i64, _sn: u64) {
        let (idx, k, v) = self.get_inputs(k80, v);
        let unit = &mut self.units[idx].write();
        unit.erase_kv(k, v);
        self.sizes[idx / SHARD_DIV].fetch_sub(1, Ordering::Relaxed);
    }

    fn change_kv(&self, k80: &[u8], v_old: i64, v_new: i64, _sn_old: u64, _sn_new: u64) {
        let (idx, k, v_old) = self.get_inputs(k80, v_old);
        let v_new = Self::shrink_value(v_new);
        let unit = &mut self.units[idx].write();
        unit.change_kv(k, v_old, v_new);
    }

    fn for_each(&self, h: i64, op: u8, k80: &[u8]) -> PosList {
        if op == OP_CREATE || op == OP_DELETE {
            return self.for_each_adjacent_value(h, k80);
        } else if op == OP_WRITE || op == OP_READ {
            // OP_READ is only for debug
            return self.for_each_value(h, k80);
        }
        panic!("Unknown op={op}");
    }

    fn for_each_value_warmup(&self, h: i64, k80: &[u8]) -> PosList {
        self.for_each_value(h, k80)
    }

    fn for_each_value(&self, _h: i64, k80: &[u8]) -> PosList {
        let (idx, k, _) = self.get_inputs(k80, 0);
        let unit = self.units[idx].read();
        let mut pos_list = PosList::default();
        unit.for_each_value(k, |v| {
            pos_list.append(Self::scale_value(v));
            false // do not exit loop
        });
        pos_list
    }

    fn for_each_adjacent_value(&self, _h: i64, k80: &[u8]) -> PosList {
        let (idx, k, _) = self.get_inputs(k80, 0);
        let unit = self.units[idx].read();
        let mut buf = [0u8; 10];
        buf[..9].copy_from_slice(&k80[..9]);
        let mut pos_list = PosList::default();
        unit.for_each_adjacent_value(buf, k, |v| {
            pos_list.append(Self::scale_value(v));
            false // do not exit loop
        });
        pos_list
    }

    fn key_exists(&self, k80: &[u8], file_pos: i64, _: u64) -> bool {
        let (idx, k, _) = self.get_inputs(k80, 0);
        let unit = self.units[idx].read();
        let mut exists = false;
        let v_ = Self::shrink_value(file_pos);
        unit.for_each_value(k, |v| {
            exists = v == v_;
            exists
        });
        exists
    }
}
