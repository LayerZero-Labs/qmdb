use super::hybrid::ref_unit::RefUnit;
use crate::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE, SHARD_COUNT, SHARD_DIV};
use aes_gcm::Aes256Gcm;
use byteorder::{BigEndian, ByteOrder, LittleEndian};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound::{Excluded, Included};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time;

// When create a new leaf, this is its initial capacity
const LEAF_INIT_CAP: usize = 8;
// During compacting, we want to make leaf.capacity() equal to this value
const DEFAULT_LEAF_CAP: usize = 32;
// We leave some free space for extra KV-pairs for future insertion
const EXTRA_KV_PAIRS: usize = 2;
const DEFAULT_LEAF_LEN: usize = DEFAULT_LEAF_CAP - EXTRA_KV_PAIRS;
// Control whether to compact a unit
const COMPACT_RATIO: u32 = 20;
const COMPACT_THRES: u32 = 500;
const COMPACT_TRY_RANGE: usize = 2000; //2000 or 20(debug)

const BUF_SIZE: usize = 1024 * 10 - 16;
const K_SIZE: usize = 6;
const V_SIZE: usize = 6;
const KV_SIZE: usize = K_SIZE + V_SIZE;

// We allocate small leaves in large chunks of memory
struct BufList {
    list: Vec<Box<[u8]>>,
    offset: usize,
}

impl BufList {
    fn new() -> Self {
        let mut list = Vec::new();
        list.push(vec![0u8; BUF_SIZE].into_boxed_slice());
        Self { list, offset: 0 }
    }

    // allocate space for a Leaf with up to 'cap' KV-pairs
    fn allocate(&mut self, cap: usize) -> (u64, &mut [u8]) {
        if cap > u16::MAX as usize {
            panic!("Leaf Capacity Too Large");
        }
        let last_len = self.list.last().unwrap().len();
        let size = cap * KV_SIZE + 4; // （k, v）= KV_LEN bytes, 2 bytes for len, 2 bytes for cap
        if self.offset + size > last_len {
            //current buffer has not enough space for allocating
            let buf_size = usize::max(BUF_SIZE, size);
            if buf_size > u32::MAX as usize {
                panic!("Buffer Size Too Large");
            }
            self.list.push(vec![0u8; buf_size].into_boxed_slice());
            self.offset = 0;
        }
        let buf_idx = self.list.len() - 1;
        let last = self.list.last_mut().unwrap();
        // write the leaf's capacity
        LittleEndian::write_u16(&mut last[self.offset..self.offset + 2], cap as u16);
        // concat 32b-buffer-index and 32b-in-buffer-offset together
        let pos = ((buf_idx as u64) << 32) | (self.offset as u64);
        let bz = &mut last[self.offset + 2..self.offset + size];
        self.offset += size;
        (pos, bz)
    }

    // get a readonly leaf
    fn get_ro_leaf(&self, pos: u64) -> RoLeaf {
        let buf_idx = (pos >> 32) as usize;
        let offset = (pos as u32) as usize;
        let buf = self.list.get(buf_idx).unwrap();
        // read the leaf's capacity
        let cap = LittleEndian::read_u16(&buf[offset..offset + 2]) as usize;
        let size = cap * KV_SIZE + 4;
        RoLeaf {
            data: &buf[offset + 2..offset + size],
        }
    }

    // get a read-write leaf
    fn get_leaf(&mut self, pos: u64) -> Leaf {
        let buf_idx = (pos >> 32) as usize;
        let offset = (pos as u32) as usize;
        let buf = self.list.get_mut(buf_idx).unwrap();
        // read the leaf's capacity
        let cap = LittleEndian::read_u16(&buf[offset..offset + 2]) as usize;
        let size = cap * KV_SIZE + 4;
        Leaf {
            data: &mut buf[offset + 2..offset + size],
        }
    }

    // allocate space for a leaf and write the first KV pair to it
    fn create_leaf(&mut self, k: u64, v: i64) -> u64 {
        let (pos, data) = self.allocate(LEAF_INIT_CAP);
        let mut res = Leaf { data };
        res.data[0] = 0;
        res.data[1] = 0;
        res.append(k, v);
        pos
    }
}

// A leaf contains a sorted KV-pair list. Both key and value are 6-byte-long.
// It contains at most 65535 KV-pairs. Two bytes are used to store the count.
struct Leaf<'a> {
    data: &'a mut [u8],
}

struct RoLeaf<'a> {
    data: &'a [u8],
}

impl RoLeaf<'_> {
    fn is_full(&self) -> bool {
        2 + self.len() * KV_SIZE == self.data.len()
    }

    // return the total count of kv-pairs that can be written
    #[cfg(test)]
    fn capacity(&self) -> usize {
        (self.data.len() - 2) / KV_SIZE
    }

    // return the number of kv-pairs that was already written
    fn len(&self) -> usize {
        LittleEndian::read_u16(&self.data[..2]) as usize
    }

    #[cfg(test)]
    fn to_vec(&self) -> Vec<(u64, i64)> {
        let len = self.len();
        let mut res = Vec::with_capacity(len);
        for i in 0..len {
            res.push(self.get(i));
        }
        res
    }

    // find the first pair whose key is no less than k
    fn seek(&self, k: u64) -> Option<usize> {
        for i in 0..self.len() {
            let (got_k, _) = self.get(i);
            if got_k >= k {
                return Some(i);
            }
        }
        None
    }

    // find a pair equaling (k, v)
    fn find_kv(&self, k: u64, v: i64) -> Option<usize> {
        for i in 0..self.len() {
            let (got_k, got_v) = self.get(i);
            if (k, v) == (got_k, got_v) {
                return Some(i);
            }
            if got_k > k {
                return None;
            }
        }
        None
    }

    // get a kv-pair at 'idx'
    fn get(&self, idx: usize) -> (u64, i64) {
        let start = 2 + idx * KV_SIZE;
        let k = LittleEndian::read_u64(&self.data[start..start + 8]) << 16;
        let v = LittleEndian::read_u64(&self.data[start + KV_SIZE - 8..start + KV_SIZE]) >> 16;
        (k >> 16, v as i64)
    }
}

impl Leaf<'_> {
    fn to_readonly(&self) -> RoLeaf {
        RoLeaf { data: &*self.data }
    }

    fn is_full(&self) -> bool {
        self.to_readonly().is_full()
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.to_readonly().capacity()
    }

    fn len(&self) -> usize {
        self.to_readonly().len()
    }

    #[cfg(test)]
    fn to_vec(&self) -> Vec<(u64, i64)> {
        self.to_readonly().to_vec()
    }

    fn seek(&self, k: u64) -> Option<usize> {
        self.to_readonly().seek(k)
    }

    fn find_kv(&self, k: u64, v: i64) -> Option<usize> {
        self.to_readonly().find_kv(k, v)
    }

    fn get(&self, idx: usize) -> (u64, i64) {
        self.to_readonly().get(idx)
    }

    // change the value of the kv-pair at 'idx'
    fn change_value(&mut self, idx: usize, v: i64) {
        let start = 2 + idx * KV_SIZE;
        let bz = v.to_le_bytes();
        self.data[start + K_SIZE..start + KV_SIZE].copy_from_slice(&bz[..6]);
    }

    // write a new kv-pair at the end
    fn append(&mut self, k: u64, v: i64) {
        self.insert(self.len(), k, v);
    }

    // insert a new kv-pair at any position (beginning/middle/end)
    fn insert(&mut self, idx: usize, k: u64, v: i64) {
        let len = self.len();
        let start = 2 + idx * KV_SIZE;
        let end = 2 + len * KV_SIZE;
        if start < end {
            self.data.copy_within(start..end, start + KV_SIZE); //make room
        }
        LittleEndian::write_u64(&mut self.data[start..start + 8], k);
        let bz = v.to_le_bytes();
        self.data[start + K_SIZE..start + KV_SIZE].copy_from_slice(&bz[..6]);
        let new_len = len as u16 + 1;
        LittleEndian::write_u16(&mut self.data[..2], new_len);
    }

    // remove a kv-pair at any position (beginning/middle/end)
    fn remove(&mut self, idx: usize) {
        let len = self.len();
        let start = 2 + idx * KV_SIZE;
        let end = 2 + len * KV_SIZE;
        self.data.copy_within(start + KV_SIZE..end, start); //squeeze empty space
        let new_len = len as u16 - 1;
        LittleEndian::write_u16(&mut self.data[..2], new_len);
    }

    fn erase(&mut self, k: u64, v: i64) {
        if let Some(idx) = self.find_kv(k, v) {
            self.remove(idx);
            return;
        }
        panic!("Cannot Erase Non-existent KV");
    }

    fn change(&mut self, k: u64, old_v: i64, new_v: i64) {
        if let Some(idx) = self.find_kv(k, old_v) {
            self.change_value(idx, new_v);
            return;
        }
        panic!("Cannot Change Non-existent KV");
    }
}

// we need 65536 such units
pub trait UnitTrait: Send + Sync {
    fn new() -> Self;
    fn add_kv(&mut self, k: u64, v: i64);
    fn erase_kv(&mut self, k: u64, v: i64);
    fn change_kv(&mut self, k: u64, v_old: i64, v_new: i64);
    fn for_each_value<F>(&self, k56: u64, access: F)
    where
        F: FnMut(i64) -> bool;
    fn for_each_adjacent_value<F>(&self, k80: [u8; 10], k56: u64, access: F)
    where
        F: FnMut(&[u8], i64) -> bool;
    fn compact(&mut self) -> bool;
    fn is_compactible(&self) -> bool;
}

pub struct Unit {
    bt: BTreeMap<u64, u64>, // 56b-key => dual-32b-position
    buf_list: BufList,
    last_size: u32,
    change_count: u32,
    temp_vec: Vec<u8>,
}

fn split_hi_lo(k: u64) -> (u64, u64) {
    let hi16 = (k >> 48) << 48; //clear low 48b
    let lo48 = (k << 16) >> 16; //clear high 16b
    (hi16, lo48)
}

impl UnitTrait for Unit {
    fn new() -> Self {
        Self {
            bt: BTreeMap::new(),
            buf_list: BufList::new(),
            last_size: 0,
            change_count: 0,
            temp_vec: Vec::with_capacity(0),
        }
    }

    fn add_kv(&mut self, k56: u64, v: i64) {
        let (hi16, k) = split_hi_lo(k56);
        // a leaf contains keys with the same hi16, so if no leaf
        // was created for this hi16, we create a new one.
        let (&leaf_key, &leaf_pos) = {
            let last = self.bt.range((Included(&hi16), Included(&k56))).next_back();
            if last.is_none() {
                //println!("create_leaf hi16={:#016x} k56={:#016x}", hi16, k56);
                self.change_count += LEAF_INIT_CAP as u32;
                let pos = self.buf_list.create_leaf(k, v);
                self.bt.insert(hi16, pos);
                return;
            }
            last.unwrap()
        };
        let mut leaf = self.buf_list.get_leaf(leaf_pos);
        //if k56 == 0x3d8cd5bf419240 {
        //    println!("add_kv leaf_key={:#016x} leaf_pos={:#016x} lo48={:#016x} v={} {:?}", leaf_key, leaf_pos, k, v, leaf.to_vec());
        //}
        let is_full = leaf.is_full();
        if let Some(idx) = leaf.seek(k) {
            if is_full {
                // split by moving out the tail part starting from 'idx'
                let count = leaf.len() - idx; // count >= 1
                let start = 2 + KV_SIZE * idx;
                self.temp_vec.clear();
                //println!("extend_from_slice {} {}", start, start + 12 * count);
                //println!("current leaf leaf_pos={:#016x} {:?}", leaf_pos, leaf.to_vec());
                self.temp_vec
                    .extend_from_slice(&leaf.data[start..start + 12 * count]);
                //change count
                LittleEndian::write_u16(&mut leaf.data[..2], idx as u16);
                //allocate a new leaf with 25% margin
                let alloc_count = 1 + count + usize::min(count / 4, 2);
                //println!("alloc_count={} idx={}", alloc_count, idx);
                let (pos, bz) = self.buf_list.allocate(alloc_count);
                //write the count
                LittleEndian::write_u16(&mut bz[..2], (1 + count) as u16);
                //write the first kv-pair
                LittleEndian::write_u64(&mut bz[2..10], k);
                let v_le = v.to_le_bytes();
                bz[8..14].copy_from_slice(&v_le[..6]);
                //write the other kv-pairs
                bz[14..14 + KV_SIZE * count].copy_from_slice(&self.temp_vec[..]);
                if idx == 0 {
                    // update current leaf
                    self.change_count += (alloc_count - count) as u32;
                    self.bt.insert(leaf_key, pos);
                } else {
                    self.change_count += alloc_count as u32;
                    // keep current, create next
                    self.bt.insert(k56, pos);
                }
            } else {
                leaf.insert(idx, k, v);
            }
        } else {
            //all the keys in the leaf is less than k
            if is_full {
                let pos = self.buf_list.create_leaf(k, v);
                self.bt.insert(k56, pos);
                self.change_count += LEAF_INIT_CAP as u32;
            } else {
                leaf.append(k, v);
                self.change_count += 1;
            }
        }
    }

    fn erase_kv(&mut self, k: u64, v: i64) {
        self.change_count += 1;
        let (hi16, lo48) = split_hi_lo(k);
        let (&leaf_key, &leaf_pos) = {
            let last = self.bt.range((Included(&hi16), Included(&k))).next_back();
            if last.is_none() {
                panic!("Cannot Erase Non-existent KV");
            }
            last.unwrap()
        };
        let mut leaf = self.buf_list.get_leaf(leaf_pos);
        leaf.erase(lo48, v);
        //if k == 0x672887d94debcb {
        //    println!("erase_kv leaf.len()={} lo48={:#016x} v={:#016x}", leaf.len(), lo48, v);
        //}
        if leaf.len() == 0 {
            self.bt.remove(&leaf_key);
        }
    }

    fn change_kv(&mut self, k: u64, v_old: i64, v_new: i64) {
        self.change_count += 1;
        let (hi16, lo48) = split_hi_lo(k);
        let (_, &leaf_pos) = {
            let last = self.bt.range((Included(&hi16), Included(&k))).next_back();
            if last.is_none() {
                panic!("Cannot Change Non-existent KV");
            }
            last.unwrap()
        };
        let mut leaf = self.buf_list.get_leaf(leaf_pos);
        leaf.change(lo48, v_old, v_new);
    }

    fn for_each_value<F>(&self, k56: u64, mut access: F)
    where
        F: FnMut(i64) -> bool,
    {
        let leaf = self.find_leaf(k56);
        if leaf.is_none() {
            return;
        }
        let leaf = leaf.unwrap();
        //println!("got leaf {:?}", leaf.to_vec());
        let (_, lo48) = split_hi_lo(k56);
        for i in 0..leaf.len() {
            let (k48, pos48) = leaf.get(i);
            if k48 < lo48 {
                continue;
            } else if k48 == lo48 {
                let stop = access(pos48);
                if stop {
                    break;
                }
            } else {
                // k48 > lo48
                break;
            }
        }
    }

    fn for_each_adjacent_value<F>(&self, mut k80: [u8; 10], k56: u64, mut access: F)
    where
        F: FnMut(&[u8], i64) -> bool,
    {
        let mut cur_key = k56;
        let mut idx = 0usize;
        let opt = self.find_key_and_leaf(k56);
        //if k80[0] == 0xc3 && k80[1] == 0xfa {
        //    println!("feav k80={} k56={:#016x} is_none={}", hex::encode(&k80), k56, opt.is_none());
        //}
        let mut target = None;
        // if we can find a leaf that may contain k56
        if opt.is_some() {
            let leaf;
            (cur_key, leaf) = opt.unwrap();
            let (_, lo48) = split_hi_lo(k56);
            let end = leaf.len();
            idx = end;
            //if k80[0] == 0xc3 && k80[1] == 0xfa {
            //    println!("curr target {:?}", leaf.to_vec());
            //}
            for i in 0..end {
                let (k48, pos48) = leaf.get(i);
                if k48 < lo48 {
                    continue;
                } else if k48 == lo48 {
                    if idx == end {
                        idx = i; // idx can only be assigned once
                    }
                    //if k80[0] == 0xc3 && k80[1] == 0xfa {
                    //    println!("HERE0 k80={} {:#016x} {}", hex::encode(&k80), k48, pos48);
                    //}
                    let stop = access(&k80[..], pos48);
                    if stop {
                        break;
                    }
                } else {
                    // k48 > lo48
                    if idx == end {
                        idx = i; // idx can only be assigned once
                    }
                    break;
                }
            }
            target = Some(leaf);
        }
        // if current KV-pair is the first in leaf, or opt is none, go to the
        // previous leaf (if any).
        if idx == 0 {
            if cur_key == 0 {
                return; // no prev
            }
            let opt = self.find_prev_key_and_leaf(cur_key);
            if opt.is_none() {
                //if k80[0] == 0xc3 && k80[1] == 0xfa {
                //    println!("NONE OPT cur_key=={:#016x}", cur_key);
                //}
                return; // no prev
            }
            let (prev_key, prev_leaf) = opt.unwrap();
            k80[2] = (prev_key >> 48) as u8;
            //println!("======= HE prev_key={:#016x} cur_key-1={:#016x} k80={}", prev_key, cur_key - 1, hex::encode(&k80));
            idx = prev_leaf.len();
            target = Some(prev_leaf);
        }
        let target = target.unwrap(); // we are sure it's Some
        idx -= 1;
        let (k_prev, _) = target.get(idx);
        let bz = k_prev.to_be_bytes();
        k80[3..9].copy_from_slice(&bz[2..]);
        //println!("HERE k80={} {:#016x}", hex::encode(&k80), k_prev);
        loop {
            let (lo48, pos48) = target.get(idx);
            //println!("AA lo48={:#016x} k_prev={:#016x} pos={}", lo48, k_prev, pos48);
            if lo48 != k_prev {
                break;
            }
            let stop = access(&k80[..], pos48);
            if idx == 0 || stop {
                break;
            }
            idx -= 1;
        }
    }

    fn is_compactible(&self) -> bool {
        self.change_count * COMPACT_RATIO > u32::max(self.last_size, COMPACT_THRES)
    }

    fn compact(&mut self) -> bool {
        //if idx % 4096 == 0 {
        //    println!("idx={} compact last_size={} size={} change_count={}", idx, self.last_size, size, self.change_count);
        //}

        let mut builder = UnitBuilder::new();
        for (k, pos) in self.bt.iter() {
            let leaf = self.buf_list.get_leaf(*pos);
            let (hi16, _) = split_hi_lo(*k);
            for i in 0..leaf.len() {
                let (lo48, v) = leaf.get(i);
                builder.append(hi16 | lo48, v);
            }
        }
        builder.flush();
        self.bt = builder.bt;
        self.buf_list.offset = builder.cur_buf.len();
        builder.total_bytes += builder.cur_buf.len();
        //builder.cur_buf.resize(BUF_SIZE, 0);
        builder.cur_buf.shrink_to_fit();
        builder.buf_list.push(builder.cur_buf.into_boxed_slice());
        self.buf_list.list = builder.buf_list;
        //if idx % 4096 == 0 {
        //    let avg = (builder.total_bytes as f64) / (builder.size as f64);
        //    println!("compact#{}.finish idx={} {}+{} -> {} bytes={} avg={}", round, idx, self.last_size, self.change_count, builder.size, builder.total_bytes, avg);
        //}
        self.last_size = builder.size as u32;
        self.change_count = 0;
        true
    }
}

impl Unit {
    fn find_prev_key_and_leaf(&self, k: u64) -> Option<(u64, RoLeaf)> {
        let last = self.bt.range((Included(&0u64), Excluded(&k))).next_back();
        if let Some((k_prev, pos)) = last {
            return Some((*k_prev, self.buf_list.get_ro_leaf(*pos)));
        }
        None
    }

    fn find_key_and_leaf(&self, k: u64) -> Option<(u64, RoLeaf)> {
        let (hi16, _) = split_hi_lo(k);
        let last = self.bt.range((Included(&hi16), Included(&k))).next_back();
        if let Some((k_prev, pos)) = last {
            //println!("find_key_and_leaf hi16={:#016x} k={:#016x} k_prev={:#016x} pos={:#016x}", hi16, k, *k_prev, pos);
            return Some((*k_prev, self.buf_list.get_ro_leaf(*pos)));
        }
        None
    }

    fn find_leaf(&self, k: u64) -> Option<RoLeaf> {
        if let Some((_, leaf)) = self.find_key_and_leaf(k) {
            Some(leaf)
        } else {
            None
        }
    }

    pub fn debug_get_values(&self, k_in: u64) -> HashSet<i64> {
        let mut res = HashSet::new();
        self.for_each_value(k_in, |v| -> bool {
            res.insert(v);
            false // do not exit loop
        });
        res
    }

    pub fn debug_get_adjacent_int_values(&self, k_in: u64) -> HashSet<(u64, i64)> {
        let mut k80 = [0u8; 10];
        BigEndian::write_u64(&mut k80[2..10], k_in << 8);
        let mut res = HashSet::new();
        self.for_each_adjacent_value(k80, k_in, |k, v| -> bool {
            let k56 = BigEndian::read_u64(&k[2..]) >> 8;
            res.insert((k56, v));
            false // do not exit loop
        });
        res
    }

    pub fn debug_get_adjacent_values(&self, k80: &[u8; 10], k_in: u64) -> HashSet<([u8; 10], i64)> {
        let mut res = HashSet::new();
        self.for_each_adjacent_value(*k80, k_in, |k, v| -> bool {
            let mut k80arr = [0u8; 10];
            k80arr[..].copy_from_slice(k);
            res.insert((k80arr, v));
            false // do not exit loop
        });
        res
    }
}

fn print_set(hash_set: &HashSet<([u8; 10], i64)>) {
    for (k80, pos) in hash_set.iter() {
        println!("k80={} pos={:#016x}", hex::encode(k80), pos);
    }
}

// only used in fuzz test
pub struct Unit4Test {
    u: Unit,
    ref_u: RefUnit,
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
        self.ref_u.add_kv(0, k << 8, v);
    }

    fn erase_kv(&mut self, k: u64, v: i64) {
        self.u.erase_kv(k, v);
        self.ref_u.erase_kv(0, k << 8, v);
    }

    fn change_kv(&mut self, k: u64, v_old: i64, v_new: i64) {
        self.u.change_kv(k, v_old, v_new);
        self.ref_u.change_kv(0, k << 8, v_old, v_new);
    }

    fn for_each_value<F>(&self, k56: u64, access: F)
    where
        F: FnMut(i64) -> bool,
    {
        let r = self.ref_u.debug_get_values(0, k56 << 8);
        let i = self.u.debug_get_values(k56);
        if r != i {
            println!("r={:?}\ni={:?}", r, i);
            panic!("for_each_value mismatch k56={:#016x}", k56);
        }
        self.u.for_each_value(k56, access);
    }

    fn for_each_adjacent_value<F>(&self, k80: [u8; 10], k56: u64, access: F)
    where
        F: FnMut(&[u8], i64) -> bool,
    {
        let r = self.ref_u.debug_get_adjacent_values(&k80, 0, k56 << 8);
        let i = self.u.debug_get_adjacent_values(&k80, k56);
        if r != i {
            println!("=====ref====== k80={}", hex::encode(k80));
            print_set(&r);
            println!("=====imp======");
            print_set(&i);
            panic!("for_each_adjacent_value mismatch");
        }
        self.u.for_each_adjacent_value(k80, k56, access);
    }

    fn is_compactible(&self) -> bool {
        self.u.is_compactible()
    }

    fn compact(&mut self) -> bool {
        self.u.compact()
    }
}

struct UnitBuilder {
    bt: BTreeMap<u64, u64>,
    buf_list: Vec<Box<[u8]>>,
    cur_buf: Vec<u8>,
    cur_hi16: u64,
    key_for_leaf: u64,
    leaf_start: usize,
    leaf_size: usize,
    last_key: u64,
    // use size and total_bytes to debug memory usage
    size: usize,
    total_bytes: usize,
}

impl UnitBuilder {
    fn new() -> Self {
        let mut cur_buf = Vec::with_capacity(BUF_SIZE);
        cur_buf.resize(4, 0);
        Self {
            bt: BTreeMap::new(),
            buf_list: Vec::new(),
            cur_buf,
            cur_hi16: u64::MAX,
            key_for_leaf: u64::MAX,
            leaf_start: 0,
            leaf_size: 0,
            last_key: u64::MAX,
            size: 0,
            total_bytes: 0,
        }
    }

    fn extra_bytes(&self) -> usize {
        let extra_count = usize::min(self.leaf_size / 8, EXTRA_KV_PAIRS);
        extra_count * 12
    }

    fn flush(&mut self) {
        if self.leaf_size == 0 {
            return;
        }
        //println!("leaf_start={} leaf_end={} cur_buf.len={} extra_bytes={}", self.leaf_start, self.cur_buf.len(), self.cur_buf.len(), self.extra_bytes());
        //make the leaf's cap larger
        self.cur_buf
            .resize(self.cur_buf.len() + self.extra_bytes(), 0);
        //write bytes capacity of leaf
        let cap = (self.cur_buf.len() - self.leaf_start - 4) / KV_SIZE;
        LittleEndian::write_u16(
            &mut self.cur_buf[self.leaf_start..self.leaf_start + 2],
            cap as u16,
        );
        //write kv-pair count in leaf
        LittleEndian::write_u16(
            &mut self.cur_buf[self.leaf_start + 2..self.leaf_start + 4],
            self.leaf_size as u16,
        );
        let _leaf = RoLeaf {
            data: &self.cur_buf[self.leaf_start + 2..],
        };
        //println!("flush leaf={:?}", leaf.to_vec());
        let pos = self.buf_list.len().checked_shl(32).unwrap() | self.leaf_start;
        self.bt.insert(self.key_for_leaf, pos as u64);
        if self.cur_buf.len() + KV_SIZE > BUF_SIZE {
            // the remained space cannot hold one more pair, so we allocate new buffer
            let mut cur_buf = Vec::with_capacity(BUF_SIZE);
            cur_buf.resize(4, 0);
            std::mem::swap(&mut cur_buf, &mut self.cur_buf);
            self.total_bytes += cur_buf.len();
            self.buf_list.push(cur_buf.into_boxed_slice());
            self.leaf_start = 0;
        } else {
            self.leaf_start = self.cur_buf.len();
            self.cur_buf.resize(self.cur_buf.len() + 4, 0);
        }
        //println!("here1 leaf_end={} cur_buf.len={}", self.cur_buf.len(), self.cur_buf.len());
        self.leaf_size = 0;
        self.last_key = u64::MAX;
    }

    fn append(&mut self, k56: u64, v: i64) {
        let (hi16, _) = split_hi_lo(k56);
        //println!("append k={:#016x} v={:#016x}", k, v);
        if hi16 != self.cur_hi16 {
            //println!("flush for hi16={:#016x}. key={:#016x}", hi16, self.key_for_leaf);
            self.flush();
            // when we first see a hi16, use it as a key for leaf
            self.key_for_leaf = hi16;
            self.cur_hi16 = hi16;
        }
        // if the current leaf is large enough and current key
        // does not fall into its range, we can create a new leaf
        let ready = self.leaf_size >= DEFAULT_LEAF_LEN
            || self.cur_buf.len() + self.extra_bytes() + KV_SIZE >= BUF_SIZE;
        let still_in_range = self.last_key == k56;
        if ready && !still_in_range {
            //println!("flush for large. key={:#016x}", self.key_for_leaf);
            self.flush();
            self.key_for_leaf = k56;
        }
        let k_le = k56.to_le_bytes();
        let v_le = v.to_le_bytes();
        self.cur_buf.extend_from_slice(&k_le[..6]);
        self.cur_buf.extend_from_slice(&v_le[..6]);
        self.size += 1;
        self.leaf_size += 1;
        //println!("here2 leaf_end={} leaf_size={}", self.cur_buf.len(), self.leaf_size);
        self.last_key = k56;
    }
}

pub struct InMemIndexerGeneric<U: UnitTrait> {
    units: Vec<RwLock<U>>,
    sizes: [AtomicUsize; SHARD_COUNT],
}

pub type InMemIndexer = InMemIndexerGeneric<Unit>;
pub type InMemIndexer4Test = InMemIndexerGeneric<Unit4Test>;

const ZERO: AtomicUsize = AtomicUsize::new(0);

impl<U: UnitTrait + 'static> InMemIndexerGeneric<U> {
    pub fn with_dir(_dir: String) -> Self {
        Self::new(1 << 16)
    }

    pub fn with_dir_and_cipher(_dir: String, _cipher: Arc<Option<Aes256Gcm>>) -> Self {
        Self::new(1 << 16)
    }

    // n is 65536 in production, and it can be smaller in test
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

    pub fn len(&self, shard_id: usize) -> usize {
        self.sizes[shard_id].load(Ordering::SeqCst)
    }

    fn get_inputs(&self, k80: &[u8], v: i64) -> (usize, u64, i64) {
        let idx = BigEndian::read_u16(&k80[..2]) as usize;
        // k56 is actually &k80[2..9]. It has 7 effective bytes
        let k56 = (BigEndian::read_u64(&k80[1..9]) << 8) >> 8;
        if v % 8 != 0 {
            panic!("value not 8x");
        }
        (idx % self.units.len(), k56, v / 8)
    }

    pub fn add_kv(&self, k80: &[u8], v: i64, _sn: u64) {
        let (idx, k56, v) = self.get_inputs(k80, v);
        let unit = &mut self.units[idx].write();
        unit.add_kv(k56, v);
        self.sizes[idx / SHARD_DIV].fetch_add(1, Ordering::SeqCst);
        //unit.compact(0, idx, false); // uncomment me to measure memory usage
    }

    pub fn erase_kv(&self, k80: &[u8], v: i64, _sn: u64) {
        let (idx, k56, v) = self.get_inputs(k80, v);
        let unit = &mut self.units[idx].write();
        unit.erase_kv(k56, v);
        self.sizes[idx / SHARD_DIV].fetch_sub(1, Ordering::SeqCst);
    }

    pub fn change_kv(&self, k80: &[u8], v_old: i64, v_new: i64, _sn_old: u64, _sn_new: u64) {
        let (idx, k56, v_old) = self.get_inputs(k80, v_old);
        if v_new % 8 != 0 {
            panic!("value not 8x");
        }
        let v_new = v_new / 8;
        let unit = &mut self.units[idx].write();
        unit.change_kv(k56, v_old, v_new);
        //unit.compact(0, idx, false); // uncomment me to measure memory usage
    }

    pub fn for_each<F>(&self, h: i64, op: u8, k80: &[u8], mut access: F)
    where
        F: FnMut(&[u8], i64) -> bool,
    {
        if op == OP_CREATE || op == OP_DELETE {
            self.for_each_adjacent_value::<F>(h, k80, access);
        } else if op == OP_WRITE || op == OP_READ {
            // OP_READ is only for debug
            self.for_each_value(h, k80, |offset| access(k80, offset));
        }
    }

    pub fn for_each_value_warmup<F>(&self, h: i64, k80: &[u8], access: F)
    where
        F: FnMut(i64) -> bool,
    {
        self.for_each_value(h, k80, access);
    }

    pub fn for_each_value<F>(&self, _h: i64, k80: &[u8], mut access: F)
    where
        F: FnMut(i64) -> bool,
    {
        let (idx, k56, _) = self.get_inputs(k80, 0);
        let unit = self.units[idx].read();
        unit.for_each_value(k56, |v| access(v * 8));
    }

    pub fn for_each_adjacent_value<F>(&self, _h: i64, k80: &[u8], mut access: F)
    where
        F: FnMut(&[u8], i64) -> bool,
    {
        let (idx, k56, _) = self.get_inputs(k80, 0);
        let unit = self.units[idx].read();
        let mut buf = [0u8; 10];
        buf[..9].copy_from_slice(&k80[..9]);
        unit.for_each_adjacent_value(buf, k56, |k, v| access(k, v * 8));
    }

    pub fn key_exists(&self, k80: &[u8], file_pos: i64, _sn: u64) -> bool {
        let mut exists = false;
        self.for_each_value(0, k80, |offset| -> bool {
            if offset == file_pos {
                exists = true;
            }
            false // do not exit loop
        });
        exists
    }

    pub fn start_compacting(indexer: Arc<Self>) {
        let len = indexer.units.len();
        Self::_start_compacting(indexer.clone(), 0, len / 2);
        Self::_start_compacting(indexer, len / 2, len);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_leaf() {
        let mut buf_list = BufList::new();
        let leaf_pos = buf_list.create_leaf(0x66554433221101, 1);
        let mut leaf = buf_list.get_leaf(leaf_pos);
        leaf.erase(0x554433221101, 1);
        assert_eq!(0, leaf.len());
        leaf.append(0x554433221101, 1);
        assert_eq!(1, leaf.len());
        leaf.append(0x554433221103, 3);
        assert_eq!(2, leaf.len());
        assert_eq!(8, leaf.capacity());
        assert_eq!(
            vec![(0x554433221101, 1), (0x554433221103, 3)],
            leaf.to_vec()
        );
        assert!(!leaf.is_full());
        leaf.insert(0, 0, 0x66554433221100);
        assert_eq!(
            vec![
                (0, 0x554433221100),
                (0x554433221101, 1),
                (0x554433221103, 3)
            ],
            leaf.to_vec()
        );
        assert_eq!((0x554433221101, 1), leaf.get(1));
        leaf.change_value(1, 10);
        assert_eq!((0x554433221101, 10), leaf.get(1));
        leaf.change(0x554433221101, 10, 11);
        assert_eq!((0x554433221101, 11), leaf.get(1));
        assert_eq!(None, leaf.seek(0x554433221104));
        assert_eq!(Some(1), leaf.seek(0x554433221101));
        assert_eq!(Some(2), leaf.seek(0x554433221102));
        leaf.insert(1, 0x554433221101, 10);
        assert_eq!(
            vec![
                (0, 0x554433221100),
                (0x554433221101, 10),
                (0x554433221101, 11),
                (0x554433221103, 3)
            ],
            leaf.to_vec()
        );
        leaf.insert(0, 0, 9);
        assert_eq!(0x554433221103, leaf.get(leaf.len() - 1).0);
        assert_eq!(
            vec![
                (0, 9),
                (0, 0x554433221100),
                (0x554433221101, 10),
                (0x554433221101, 11),
                (0x554433221103, 3)
            ],
            leaf.to_vec()
        );
        leaf.remove(1);
        assert_eq!(
            vec![
                (0, 9),
                (0x554433221101, 10),
                (0x554433221101, 11),
                (0x554433221103, 3)
            ],
            leaf.to_vec()
        );
        leaf.remove(0);
        assert_eq!(
            vec![
                (0x554433221101, 10),
                (0x554433221101, 11),
                (0x554433221103, 3)
            ],
            leaf.to_vec()
        );
        leaf.insert(0, 0, 9);
        leaf.remove(3);
        assert_eq!(
            vec![(0, 9), (0x554433221101, 10), (0x554433221101, 11)],
            leaf.to_vec()
        );
        leaf.insert(3, 0x554433221103, 3);
        assert_eq!(
            vec![
                (0, 9),
                (0x554433221101, 10),
                (0x554433221101, 11),
                (0x554433221103, 3)
            ],
            leaf.to_vec()
        );
        leaf.insert(3, 0x554433221102, 2);
        leaf.insert(0, 0, 0);
        leaf.insert(6, 0x554433221104, 4);
        assert_eq!(
            vec![
                (0, 0),
                (0, 9),
                (0x554433221101, 10),
                (0x554433221101, 11),
                (0x554433221102, 2),
                (0x554433221103, 3),
                (0x554433221104, 4)
            ],
            leaf.to_vec()
        );
        assert_eq!(0x554433221104, leaf.get(leaf.len() - 1).0);
        assert!(!leaf.is_full());
        assert_eq!(7, leaf.len());
        leaf.insert(1, 0, 1);
        assert!(leaf.is_full());
        assert_eq!(8, leaf.len());
        assert_eq!(
            vec![
                (0, 0),
                (0, 1),
                (0, 9),
                (0x554433221101, 10),
                (0x554433221101, 11),
                (0x554433221102, 2),
                (0x554433221103, 3),
                (0x554433221104, 4)
            ],
            leaf.to_vec()
        );
        leaf.erase(0x554433221104, 4);
        assert_eq!(
            vec![
                (0, 0),
                (0, 1),
                (0, 9),
                (0x554433221101, 10),
                (0x554433221101, 11),
                (0x554433221102, 2),
                (0x554433221103, 3)
            ],
            leaf.to_vec()
        );
        leaf.change(0, 1, 5);
        leaf.change(0, 0, 2);
        leaf.change(0x554433221103, 3, 33);
        assert_eq!(
            vec![
                (0, 2),
                (0, 5),
                (0, 9),
                (0x554433221101, 10),
                (0x554433221101, 11),
                (0x554433221102, 2),
                (0x554433221103, 33)
            ],
            leaf.to_vec()
        );
    }

    const N: usize = 64;

    #[test]
    fn test_unit() {
        let mut unit = Unit::new();
        // the leaf#1
        unit.add_kv(0x10554433221100, 0);
        let mut leaf = unit.find_leaf(0);
        assert!(leaf.is_none());
        leaf = unit.find_leaf(0x10000000000000);
        assert_eq!(vec![(0x554433221100, 0)], leaf.unwrap().to_vec());
        unit.add_kv(0x10554433221101, 1);
        for i in 1..N {
            unit.add_kv(0x10554433221100, i as i64);
        }
        unit.add_kv(0x10554433221100, N as i64);
        // the leaf#2
        unit.add_kv(0x10554433221102, 0);
        //leaf = unit.find_leaf(0x10554433221102);
        //assert_eq!(vec![(0x554433221102, 0)], leaf.unwrap().to_vec());
        // the leaf#0
        unit.add_kv(0xAA4433221101, 0);
        //leaf = unit.find_leaf(0);
        //assert_eq!(vec![(0xAA4433221101, 0)], leaf.unwrap().to_vec());
        // the leaf#3
        unit.add_kv(0x11554433221100, 22);
        //leaf = unit.find_leaf(0x11554433221102);
        //assert_eq!(vec![(0x554433221100, 22)], leaf.unwrap().to_vec());
        unit.add_kv(0x11554433221101, 23);
        //leaf = unit.find_leaf(0x11554433221102);
        //assert_eq!(vec![(0x554433221100, 22), (0x554433221101, 23)], leaf.unwrap().to_vec());

        let mut hash_set = HashSet::new();
        for i in 0..(N + 1) {
            hash_set.insert(i as i64);
        }
        let mut values = unit.debug_get_values(0x10554433221100);
        assert_eq!(hash_set, values);

        // remove 5 and 8, change 11 to 101
        unit.erase_kv(0x10554433221100, 5);
        hash_set.remove(&5i64);
        values = unit.debug_get_values(0x10554433221100);
        assert_eq!(hash_set, values);
        unit.erase_kv(0x10554433221100, 8);
        hash_set.remove(&8i64);
        values = unit.debug_get_values(0x10554433221100);
        assert_eq!(hash_set, values);
        unit.change_kv(0x10554433221100, 11, 101);
        hash_set.remove(&11i64);
        hash_set.insert(101);
        values = unit.debug_get_values(0x10554433221100);
        assert_eq!(hash_set, values);

        // check leaf#3
        hash_set.clear();
        hash_set.insert(22);
        values = unit.debug_get_values(0x11554433221100);
        assert_eq!(hash_set, values);
        hash_set.clear();
        hash_set.insert(23);
        values = unit.debug_get_values(0x11554433221101);
        assert_eq!(hash_set, values);

        hash_set.clear();
        values = unit.debug_get_values(0x08554433221100);
        assert_eq!(hash_set, values);

        check_unit(&unit);
        unit.compact();
        check_unit(&unit);
    }

    fn check_unit(unit: &Unit) {
        let mut hash_set = HashSet::<(u64, i64)>::new();
        hash_set.insert((0x11554433221101, 23));
        hash_set.insert((0x11554433221100, 22));
        let mut values = unit.debug_get_adjacent_int_values(0x11554433221101);
        assert_eq!(hash_set, values);
        values = unit.debug_get_adjacent_int_values(0x11554433221100);
        hash_set.remove(&(0x11554433221101, 23));
        hash_set.insert((0x10554433221102, 0)); //to leaf#2
        assert_eq!(hash_set, values);

        // check leaf#2 and leaf#1
        hash_set.clear();
        hash_set.insert((0x10554433221102, 0));
        hash_set.insert((0x10554433221101, 1));
        values = unit.debug_get_adjacent_int_values(0x10554433221102);
        assert_eq!(hash_set, values);

        values = unit.debug_get_adjacent_int_values(0x10554433221101);
        hash_set.remove(&(0x10554433221102, 0));
        for i in 0..(N + 1) {
            if i != 5 && i != 8 && i != 11 {
                hash_set.insert((0x10554433221100, i as i64));
            }
        }
        hash_set.insert((0x10554433221100, 101));
        assert_eq!(hash_set, values);

        // check leaf#1 and leaf#0
        hash_set.remove(&(0x10554433221101, 1));
        hash_set.insert((0xAA4433221101, 0));
        values = unit.debug_get_adjacent_int_values(0x10554433221100);
        assert_eq!(hash_set, values);

        // check leaf#0
        hash_set.clear();
        hash_set.insert((0xAA4433221101, 0));
        values = unit.debug_get_adjacent_int_values(0xAAB433221101);
        assert_eq!(hash_set, values);
        values = unit.debug_get_adjacent_int_values(0x08554433221100);
        assert_eq!(hash_set, values);
    }
}
