use crate::test_helper::RandSrc;
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_WRITE, SENTRY_COUNT, SHARD_COUNT};
use qmdb_common::entryfile::entry::entry_key_hash;
use qmdb_common::utils::changeset::ChangeSet;

use crate::tasks::TasksManager;
use crate::test_helper::SimpleTask;
use parking_lot::RwLock;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::ops::Bound::{self, Included};
use std::sync::Arc;

const BUF_LEN: usize = 100;
const EPOCH_FILE: &str = "./data/blocks/epoch";
pub const TOTAL_SENTRY: usize = SENTRY_COUNT * SHARD_COUNT;

/// Partially fill a preallocated key buffer deterministically based on `n`.
///
/// Returns the filled slice of the buffer that represents the key bytes.
pub fn fill_key(key_buf: &mut [u8], n: u32) -> &[u8] {
    // Deterministic key length: 20 or 52 bytes
    let len = if (n as usize & 3) == 0 { 20 } else { 52 };
    let bz = n.to_be_bytes();
    for i in 0..len / 4 {
        key_buf[i * 4..i * 4 + 4].copy_from_slice(&bz[..]);
    }
    &key_buf[..len]
}

/// Fill a preallocated value buffer with all bytes of `v` repeated, length derived from `n`.
///
/// Returns the slice of the buffer that represents the value bytes.
pub fn fill_value(value: &mut [u8], n: u32, v: u32) -> &[u8] {
    let len = 1 + (n as usize / 16) % 80; // 1~81
    let bz = v.to_be_bytes();
    for (i, item) in value.iter_mut().take(len).enumerate() {
        *item = bz[i % 4];
    }
    &value[..len]
}

/// A single key/value operation emitted by the fuzz generator.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Op {
    /// Key identifier. Also influences generated key length.
    pub key: u32,
    /// New Value
    pub value: u32,
    /// Old Value (for OP_WRITE and OP_DELETE)
    pub old_value: u32,
    /// Operation code (`OP_CREATE`, `OP_WRITE`, `OP_DELETE`).
    pub op: u8,
}

/// Split a vector into `n` contiguous parts whose lengths differ by at most 1.
///
/// - Preserves the original element order.
/// - If `n > vec.len()`, some trailing parts will be empty.
/// - If `n == 0`, returns an empty list of parts.
pub fn split_vec(vec: &[(u32, u32)], n: usize) -> Vec<Vec<(u32, u32)>> {
    if n == 0 {
        return Vec::new();
    }

    let total_len = vec.len();
    let base = total_len / n;
    let remainder = total_len % n;

    let mut iter = vec.iter();
    let mut parts: Vec<Vec<(u32, u32)>> = Vec::with_capacity(n);
    for i in 0..n {
        let take = if i < remainder { base + 1 } else { base };
        let mut part = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(&v) = iter.next() {
                part.push(v);
            }
        }
        parts.push(part);
    }
    parts
}

/// A block of work to be applied at a given simulated height.
///
/// - `tasks`: list of tasks; each task is a list of change sets (Vec<Op>).
/// - `read_list`: keys and expected byte values that must be readable after apply.
/// - `miss_list`: keys that must not exist.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Block {
    /// All tasks scheduled in the block (task -> list of change sets -> ops).
    pub tasks: Vec<Vec<Vec<Op>>>,
    /// Keys that should be readable with the expected value.
    pub read_list: Vec<(u32, u32)>,
    /// Keys that should not be found.
    pub miss_list: Vec<(u32, u32)>,
    /// Simulated block height used to derive task ids and value epochs.
    pub height: u32,
}

impl Block {
    /// Construct a `SimpleTask` for task index `i`.
    pub fn get_task(&self, i: usize) -> SimpleTask {
        let mut change_sets = Vec::with_capacity(self.tasks[i].len());
        for op_list in self.tasks[i].iter() {
            if op_list.len() != 0 {
                change_sets.push(Self::get_change_set(&op_list[..]));
            }
        }
        SimpleTask::new(change_sets)
    }

    pub fn get_taskman(&self) -> Arc<TasksManager<SimpleTask>> {
        let len = self.tasks.len();
        let task_list: Vec<_> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, _)| RwLock::new(Some(self.get_task(i))))
            .collect();
        Arc::new(TasksManager::new(task_list))
    }

    pub fn get_change_set(op_list: &[Op]) -> ChangeSet {
        let mut cset = ChangeSet::new();
        let mut key_bz = [0u8; BUF_LEN];
        let mut value_bz = [0u8; BUF_LEN];
        for op in op_list.iter() {
            let k = fill_key(&mut key_bz[..], op.key);
            let v = fill_value(&mut value_bz[..], op.key, op.value as u32);
            let kh = entry_key_hash(k);
            cset.add_op(op.op, &kh, k, v, None);
        }
        cset.sort();
        cset
    }
}

pub fn rand_between(randsrc: &mut RandSrc, min: usize, max: usize) -> usize {
    let span = max - min;
    min + randsrc.get_uint32() as usize % span
}

pub struct TestGenV0 {
    pub blocks_in_epoch: usize,
    pub change_set_size_min: usize,
    pub change_set_size_max: usize,
    pub max_cset_in_task: usize,
    pub drop_prob: usize,
    pub block_size_min: usize,
    pub block_size_max: usize,
    pub active_num_to_start_remove: usize,
    pub active_num_to_start_read: usize,
    pub active_num_to_stop_add: usize,
    pub write_prob: usize,
    pub del_prob: usize,
    pub add_prob: usize,
    pub recent_prob: usize,
    pub read_count_min: usize,
    pub read_count_max: usize,
    pub miss_count_min: usize,
    pub miss_count_max: usize,
    pub randsrc: RandSrc,
    pub tree: BTreeMap<u32, u32>,
    pub height: u32,
    pub curr_del_set: BTreeSet<u32>,
    pub prev_del_set: BTreeSet<u32>,
    pub curr_add_set: BTreeSet<u32>,
    pub prev_add_set: BTreeSet<u32>,
    pub curr_wr_set: BTreeSet<u32>,
    pub prev_wr_set: BTreeSet<u32>,
}

impl TestGenV0 {
    pub fn new(randsrc: RandSrc) -> Self {
        Self {
            blocks_in_epoch: 1000,
            change_set_size_min: 5,
            change_set_size_max: 15,
            max_cset_in_task: 20,
            drop_prob: 2,
            block_size_min: 10,
            block_size_max: 100,
            active_num_to_start_remove: TOTAL_SENTRY + (3 << 14),
            active_num_to_start_read: TOTAL_SENTRY + (3 << 8),
            active_num_to_stop_add: 1 << 22,
            write_prob: 40,
            del_prob: 10,
            add_prob: 20,
            recent_prob: 10,
            read_count_min: 1000,
            read_count_max: 2000,
            miss_count_min: 1000,
            miss_count_max: 2000,
            randsrc,
            tree: BTreeMap::new(),
            height: 1,
            curr_del_set: BTreeSet::new(),
            prev_del_set: BTreeSet::new(),
            curr_add_set: BTreeSet::new(),
            prev_add_set: BTreeSet::new(),
            curr_wr_set: BTreeSet::new(),
            prev_wr_set: BTreeSet::new(),
        }
    }

    pub fn gen_epochs(&mut self, epoch_count: usize) {
        let mut blocks = Vec::with_capacity(self.blocks_in_epoch);
        for epoch_id in 0..epoch_count {
            println!("epoch#{}", epoch_id);
            self.gen_blocks(epoch_id, &mut blocks);
            println!("epoch#{} done", epoch_id);
        }
    }

    pub fn gen_blocks(&mut self, epoch_id: usize, blocks: &mut Vec<Block>) {
        blocks.clear();
        println!("Why blocks_in_epoch: {}", self.blocks_in_epoch);
        for i in 0..self.blocks_in_epoch {
            let height = self.height;
            println!("block#{}", height);
            blocks.push(self.gen_block());
            println!("block#{} done", height);
        }
        let out = bincode::serialize(blocks).unwrap();
        let data_file = format!("{}_{}", EPOCH_FILE, epoch_id);
        let mut file = File::create_new(data_file).unwrap();
        file.write_all(out.as_ref()).unwrap();
    }

    pub fn gen_block(&mut self) -> Block {
        std::mem::swap(&mut self.prev_del_set, &mut self.curr_del_set);
        self.curr_del_set.clear();
        std::mem::swap(&mut self.prev_add_set, &mut self.curr_add_set);
        self.curr_add_set.clear();
        std::mem::swap(&mut self.prev_wr_set, &mut self.curr_wr_set);
        self.curr_wr_set.clear();
        let blk_size = rand_between(&mut self.randsrc, self.block_size_min, self.block_size_max);
        let reads = rand_between(
            &mut self.randsrc,
            self.read_count_min,
            self.read_count_max + 1,
        );
        let misses = rand_between(
            &mut self.randsrc,
            self.miss_count_min,
            self.miss_count_max + 1,
        );
        let mut blk = Block {
            height: self.height,
            tasks: Vec::with_capacity(blk_size),
            read_list: Vec::with_capacity(reads),
            miss_list: Vec::with_capacity(misses),
        };
        self.height += 1;
        for i in 0..blk_size {
            //println!("tx#{}", i);
            let count = rand_between(&mut self.randsrc, 1, self.max_cset_in_task);
            let mut task = Vec::with_capacity(count);
            for j in 0..count {
                //println!("changeset#{}", j);
                let drop = rand_between(&mut self.randsrc, 0, 100);
                if drop < self.drop_prob {
                    task.push(vec![]); // a dropped tx
                } else {
                    task.push(self.gen_cset());
                }
                //println!("changeset#{} done", j);
            }
            blk.tasks.push(task);
            //println!("tx#{} done", i);
        }
        if self.height > 10 {
            for i in 0..reads {
                blk.read_list.push(self.gen_read());
            }
            //println!("read_list@blk#{} done", self.height);
            for _ in 0..misses {
                blk.miss_list.push((self.gen_miss(), 0));
            }
            //println!("miss_list@blk#{} done", self.height);
        }
        blk
    }

    fn gen_cset(&mut self) -> Vec<Op> {
        let cset_size = rand_between(
            &mut self.randsrc,
            self.change_set_size_min,
            self.change_set_size_max,
        );
        let mut keys = HashSet::new();
        let mut cset = Vec::with_capacity(cset_size);
        for _ in 0..cset_size {
            cset.push(self.gen_op(&mut keys));
            //println!("gen_op done");
        }
        cset
    }

    pub fn get_rand_kv_with_recent(&mut self, keys: &mut HashSet<u32>) -> (u32, u32) {
        let recent = rand_between(&mut self.randsrc, 0, 100);
        self._get_rand_kv(keys, recent < self.recent_prob)
    }

    pub fn get_rand_kv(&mut self, keys: &mut HashSet<u32>) -> (u32, u32) {
        self._get_rand_kv(keys, false)
    }

    pub fn get_recent_rand_key(&mut self) -> u32 {
        let btree_sets = vec![
            &self.curr_del_set,
            &self.prev_del_set,
            &self.curr_add_set,
            &self.prev_add_set,
            &self.curr_wr_set,
            &self.prev_wr_set,
        ];
        let mut i = self.randsrc.get_uint32() as usize % 6;
        loop {
            let n = self.randsrc.get_uint32();
            let range = (Included(&0), Included(&n));
            let prev = btree_sets[i % 6]
                .range::<u32, (Bound<&u32>, Bound<&u32>)>(range)
                .next_back();
            if let Some(prev) = prev {
                return *prev;
            }
            i += 1;
        }
    }

    pub fn _get_rand_kv(&mut self, keys: &mut HashSet<u32>, with_recent: bool) -> (u32, u32) {
        loop {
            let n = if with_recent {
                self.randsrc.get_uint32()
            } else {
                self.get_recent_rand_key()
            };
            let range = (Included(&0), Included(&n));
            let prev = self
                .tree
                .range::<u32, (Bound<&u32>, Bound<&u32>)>(range)
                .next_back();
            if let Some(prev) = prev {
                if keys.contains(prev.0) {
                    continue;
                }
                keys.insert(*prev.0);
                return (*prev.0, *prev.1);
            }
        }
    }

    pub fn gen_write(&mut self, keys: &mut HashSet<u32>) -> Op {
        loop {
            let (k, v) = self.get_rand_kv_with_recent(keys);
            let value = v ^ self.randsrc.get_uint32(); // & 0xFF;
            self.tree.insert(k, value);
            self.curr_wr_set.insert(k);
            //println!("WR k={} v={}", k, v);
            return Op {
                key: k,
                value,
                old_value: v,
                op: OP_WRITE,
            };
        }
    }

    pub fn gen_del(&mut self, keys: &mut HashSet<u32>) -> Op {
        loop {
            let (k, v) = self.get_rand_kv_with_recent(keys);
            self.tree.remove(&k);
            self.curr_del_set.insert(k);
            //println!("DEL k={}", k);
            return Op {
                key: k,
                value: 0,
                old_value: v,
                op: OP_DELETE,
            };
        }
    }

    pub fn gen_add(&mut self, keys: &mut HashSet<u32>) -> Op {
        loop {
            let k = self.randsrc.get_uint32();
            if self.tree.contains_key(&k) || keys.contains(&k) {
                continue;
            }
            keys.insert(k);
            let value = self.randsrc.get_uint32(); // & 0xFF;
            self.tree.insert(k, value);
            self.curr_add_set.insert(k);
            //println!("ADD k={} v={}", k, value);
            return Op {
                key: k,
                value,
                old_value: 0,
                op: OP_CREATE,
            };
        }
    }

    pub fn gen_read(&mut self) -> (u32, u32) {
        loop {
            let k = self.randsrc.get_uint32();
            let range = (Included(&0), Included(&k));
            let prev = self
                .tree
                .range::<u32, (Bound<&u32>, Bound<&u32>)>(range)
                .next_back();
            if let Some(prev) = prev {
                if self.curr_wr_set.contains(prev.0)
                    || self.prev_wr_set.contains(prev.0)
                    || self.curr_add_set.contains(prev.0)
                    || self.prev_add_set.contains(prev.0)
                {
                    continue;
                }
                return (*prev.0, *prev.1);
            }
        }
    }

    pub fn gen_miss(&mut self) -> u32 {
        loop {
            let k = self.randsrc.get_uint32();
            if self.tree.contains_key(&k)
                || self.curr_del_set.contains(&k)
                || self.prev_del_set.contains(&k)
            {
                continue;
            }
            return k;
        }
    }

    pub fn gen_op(&mut self, keys: &mut HashSet<u32>) -> Op {
        let (mut wr_prob, mut del_prob, mut add_prob) =
            (self.write_prob, self.del_prob, self.add_prob);
        if self.tree.len() < self.active_num_to_start_remove {
            del_prob = 0;
            wr_prob = 0;
        }
        if self.tree.len() > self.active_num_to_stop_add {
            add_prob = 0;
        }
        let mut n = self.randsrc.get_uint64() as usize;
        n %= wr_prob + del_prob + add_prob;
        if n < wr_prob {
            //println!("gen_write");
            self.gen_write(keys)
        } else if n < wr_prob + del_prob {
            //println!("gen_del");
            self.gen_del(keys)
        } else {
            //println!("gen_add");
            self.gen_add(keys)
        }
    }
}
