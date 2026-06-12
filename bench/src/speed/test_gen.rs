use byteorder::{BigEndian, ByteOrder};
use parking_lot::RwLock;
use qmdb::test_helper::{RandSrc, SimpleTask};
use qmdb_common::{
    def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE},
    utils::changeset::ChangeSet,
};
use std::collections::HashSet;
use std::sync::Arc;

use super::shuffle_param::ShuffleParam;

const PRIME2: u64 = 888869;

#[derive(Debug, Clone)]
pub struct TestGenV2 {
    pub wr_op_in_cset: usize,
    pub rd_op_in_cset: usize,
    pub delete_op_in_cset: usize,
    pub create_op_in_cset: usize,
    pub cset_in_task: usize,
    pub task_in_block: usize,
    pub cur_read_num: u64,
    pub cur_delete_num: u64,
    pub max_delete_num: u64,
    pub cur_create_num: u64,
    pub cur_num: u64,
    pub cur_round: usize,
    block_count: u64,
    pub sp: ShuffleParam,
}

// TODO: Refactor this to avoid using different rounds for populating the database and benchmarking TPS
impl TestGenV2 {
    pub fn new(
        randsrc: &mut RandSrc,
        entry_count: u64,
        cset_in_task: usize,
        task_in_block: usize,
    ) -> Self {
        // adjust entry_count to a "will-not-be-deleted" count
        let entry_count_wo_del = entry_count * 9 / 10;
        let mut sp = ShuffleParam::new(entry_count, entry_count_wo_del);
        sp.add_num = randsrc.get_uint64();
        sp.xor_num = randsrc.get_uint64();
        sp.rotate_bits = randsrc.get_uint32() as usize % sp.total_bits;

        Self {
            // Composition of changeset based on historical Ethereum
            wr_op_in_cset: 9,
            rd_op_in_cset: 15,
            delete_op_in_cset: 1,
            create_op_in_cset: 1,
            // Number of changesets in a task
            cset_in_task,
            // Number of tasks in a block
            task_in_block,
            // Initialize to 0
            cur_read_num: 0,
            cur_delete_num: 0,
            max_delete_num: 0,
            cur_create_num: 0,
            cur_num: 0,
            cur_round: 0,
            block_count: 0,
            sp,
        }
    }

    pub fn num_txn_in_blk(&self) -> u64 {
        self.task_in_block as u64 * self.cset_in_task as u64
    }

    pub fn num_ops_in_cset(&self) -> u64 {
        // This is for the first round, where we are populating the database
        self.wr_op_in_cset as u64 + self.delete_op_in_cset as u64
    }

    pub fn num_op_in_blk(&self) -> u64 {
        self.task_in_block as u64 * self.cset_in_task as u64 * self.num_ops_in_cset()
    }

    pub fn block_in_round(&self) -> u64 {
        let op_in_blk = self.num_op_in_blk();
        if self.sp.entry_count % op_in_blk != 0 {
            panic!(
                "invalid entry_count {} % {} != 0",
                self.sp.entry_count, op_in_blk
            );
        }
        let result = self.sp.entry_count / op_in_blk;
        if result == 0 {
            panic!("entry_count not enough");
        }
        result
    }

    pub fn gen_block(&mut self) -> Vec<RwLock<Option<SimpleTask>>> {
        let blk_in_round = self.block_in_round();
        // if self.block_count != 0 && self.block_count % blk_in_round == 0 {
        if self.block_count == blk_in_round {
            // First round is to populate the database, second round is to benchmark TPS.
            // Code does not currently support more than 2 rounds. TODO: Refactor this.
            // println!(
            //     "New round: {} -> {}, cur_num: {}, max_delete_num: {}, cur_delete_num: {}, cur_create_num: {}, cur_read_num: {}",
            //     self.cur_round, self.cur_round+1, self.cur_num, self.max_delete_num, self.cur_delete_num, self.cur_create_num, self.cur_read_num
            // );
            self.cur_round += 1;
            self.cur_num = 0;
            self.max_delete_num = self.cur_delete_num;
            self.cur_delete_num = 0;
            self.cur_read_num = 0;
        }
        //println!("AA gen_block cur_round={} block_count={} sp={:?}", self.cur_round, self.block_count, self.sp);
        let mut res: Vec<RwLock<Option<SimpleTask>>> = Vec::with_capacity(self.task_in_block);
        for _ in 0..self.task_in_block {
            res.push(RwLock::new(None));
        }

        let task_group_size = self.task_in_block / 8;
        std::thread::scope(|s| {
            for i in 0..8 {
                let mut tgen = self.clone();
                self.skip_tasks(task_group_size as u64);
                let start = i * task_group_size;
                let res = &res;
                s.spawn(move || {
                    for j in 0..task_group_size {
                        let idx = start + j;
                        let mut task = res[idx].write();
                        *task = Some(tgen.gen_task());
                    }
                });
            }
        });
        self.block_count += 1;
        res
    }

    /// Generate a whole block and split it into several subblocks of `subblock_size`.
    /// Each subblock is an `Arc<Vec<Vec<ChangeSet>>>` suitable for `Msg::Payload`.
    /// 
    /// Uses parallel generation with up to 8 worker threads for Round 0 (database population).
    /// Round 1+ uses serial generation because `gen_cset()` has non-deterministic state
    /// advancement due to HashSet collision handling, which makes `skip_tasks()` inaccurate.
    pub fn gen_block_as_subblocks(&mut self, subblock_size: usize) -> Vec<Arc<Vec<Vec<ChangeSet>>>> {
        let blk_in_round = self.block_in_round();
        if self.block_count == blk_in_round {
            self.cur_round += 1;
            self.cur_num = 0;
            self.max_delete_num = self.cur_delete_num;
            self.cur_delete_num = 0;
            self.cur_read_num = 0;
        }

        let num_subblocks = (self.task_in_block + subblock_size - 1) / subblock_size;
        
        // Round 1+ has non-deterministic state advancement due to HashSet collision handling
        // in gen_cset(), which makes skip_tasks() inaccurate. Force serial generation for correctness.
        // Round 0 (database population) is the performance bottleneck, so parallel is still beneficial.
        let num_workers = if self.cur_round == 0 {
            std::cmp::min(8, num_subblocks)
        } else {
            1 // Force serial for Round 1+
        };
        
        if num_workers <= 1 {
            // Serial generation for small blocks or Round 1+
            let mut res = Vec::with_capacity(num_subblocks);
            for i in 0..num_subblocks {
                let start = i * subblock_size;
                let end = std::cmp::min(start + subblock_size, self.task_in_block);
                let count = end - start;
                res.push(self.gen_subblock(count));
            }
            self.block_count += 1;
            return res;
        }
        
        // Pre-allocate result slots with RwLock for parallel writes
        let result_slots: Vec<RwLock<Option<Arc<Vec<Vec<ChangeSet>>>>>> = 
            (0..num_subblocks).map(|_| RwLock::new(None)).collect();
        
        // Calculate subblocks per worker
        let subblocks_per_worker = (num_subblocks + num_workers - 1) / num_workers;
        
        std::thread::scope(|s| {
            let mut subblock_offset = 0usize;
            
            for _worker_id in 0..num_workers {
                if subblock_offset >= num_subblocks {
                    break;
                }
                
                // Calculate how many subblocks this worker handles
                let worker_subblocks = std::cmp::min(subblocks_per_worker, num_subblocks - subblock_offset);
                
                // Calculate total tasks for this worker's subblocks
                let mut worker_tasks = 0usize;
                for sb_idx in subblock_offset..(subblock_offset + worker_subblocks) {
                    let sb_start = sb_idx * subblock_size;
                    let sb_end = std::cmp::min(sb_start + subblock_size, self.task_in_block);
                    worker_tasks += sb_end - sb_start;
                }
                
                // Clone generator state for this worker
                let mut tgen = self.clone();
                
                // Advance main generator state past this worker's tasks
                self.skip_tasks(worker_tasks as u64);
                
                let worker_start_subblock = subblock_offset;
                let worker_end_subblock = subblock_offset + worker_subblocks;
                let result_slots = &result_slots;
                let task_in_block = self.task_in_block;
                
                s.spawn(move || {
                    for sb_idx in worker_start_subblock..worker_end_subblock {
                        let sb_start = sb_idx * subblock_size;
                        let sb_end = std::cmp::min(sb_start + subblock_size, task_in_block);
                        let count = sb_end - sb_start;
                        
                        // Generate tasks for this subblock
                        let tasks: Vec<Vec<ChangeSet>> = (0..count)
                            .map(|_| {
                                let mut v = Vec::with_capacity(tgen.cset_in_task);
                                for _ in 0..tgen.cset_in_task {
                                    v.push(tgen.gen_cset());
                                }
                                v
                            })
                            .collect();
                        
                        let mut slot = result_slots[sb_idx].write();
                        *slot = Some(Arc::new(tasks));
                    }
                });
                
                subblock_offset += worker_subblocks;
            }
        });
        
        // Collect results in order
        let res: Vec<Arc<Vec<Vec<ChangeSet>>>> = result_slots
            .into_iter()
            .map(|slot| slot.into_inner().expect("subblock should be generated"))
            .collect();

        self.block_count += 1;
        res
    }

    /// Generate a subblock containing `count` tasks.
    pub fn gen_subblock(&mut self, count: usize) -> Arc<Vec<Vec<ChangeSet>>> {
        let mut tasks = Vec::with_capacity(count);
        for _ in 0..count {
            let mut v = Vec::with_capacity(self.cset_in_task);
            for _ in 0..self.cset_in_task {
                v.push(self.gen_cset());
            }
            tasks.push(v);
        }
        Arc::new(tasks)
    }

    /// Advance to Round 1 after Round 0 completes.
    /// Call this after streaming Round 0 to properly initialize state for Round 1.
    pub fn advance_to_round1(&mut self) {
        if self.cur_round == 0 {
            // Calculate what cur_delete_num would be after Round 0
            // Round 0 generates blk_in_round blocks, each with task_in_block tasks,
            // each task has cset_in_task changesets, each changeset has delete_op_in_cset deletes
            let blk_in_round = self.block_in_round();
            let total_delete_ops = blk_in_round * (self.task_in_block as u64) 
                * (self.cset_in_task as u64) * (self.delete_op_in_cset as u64);
            
            self.cur_round = 1;
            self.max_delete_num = total_delete_ops;
            self.cur_num = 0;
            self.cur_delete_num = 0;
            self.cur_read_num = 0;
            self.cur_create_num = 0;
            // Set block_count > blk_in_round to prevent gen_block_as_subblocks from
            // triggering another round transition (which would reset max_delete_num to 0)
            self.block_count = blk_in_round + 1;
        }
    }

    pub fn skip_tasks(&mut self, n: u64) {
        let m = n * (self.cset_in_task as u64);
        self.cur_num += m * (self.wr_op_in_cset as u64);
        if self.cur_round == 0 || self.max_delete_num == 0 {
            self.cur_delete_num += m * (self.delete_op_in_cset as u64);
        } else {
            self.cur_delete_num = (self.cur_delete_num
                + m * (self.delete_op_in_cset as u64) * PRIME2)
                % self.max_delete_num;
            self.cur_create_num += m * (self.create_op_in_cset as u64);
            self.cur_read_num += m * (self.rd_op_in_cset as u64);
        }
    }

    fn gen_task(&mut self) -> SimpleTask {
        let mut v = Vec::with_capacity(self.cset_in_task);
        for _ in 0..self.cset_in_task {
            v.push(self.gen_cset());
        }
        SimpleTask::new(v)
    }

    pub fn fill_kv(&self, num: u64, k: &mut [u8], v: &mut [u8]) -> [u8; 32] {
        BigEndian::write_u64(&mut k[12..20], num);
        let hash = qmdb_common::entryfile::entry::entry_key_hash(&k[12..20]);
        for i in 0..12 {
            // pseudo-random bytes derived from hash of num.
            // Equivalent to the old formula (k[20+i] ^ k[32+i]) since k[20..52] == hash.
            k[i] = hash[i] ^ hash[12 + i];
        }
        let kh = qmdb_common::entryfile::entry::entry_key_hash(&k[..20]);

        BigEndian::write_u32(&mut v[..4], self.cur_round as u32);
        v[4..].copy_from_slice(&kh[4..]);
        kh
    }

    fn gen_cset(&mut self) -> ChangeSet {
        // When we are populating the database, we do 9 creates + 1 delete.
        // When we are benchmarking TPS, we do 9 updates + 1 delete + 1 create + 15 reads.
        //
        // Writes / reads in the TPS round both draw from the same stable range
        // [0, entry_count_wo_del). `ChangeSet::sort` requires that a single
        // changeset must not contain duplicate key hashes, so we must ensure
        // that the logical key numbers used for writes and reads within this
        // changeset are disjoint.
        //
        // To achieve this, we keep a temporary `HashSet` of the stable-range
        // logical numbers used in this changeset. When generating read
        // operations, we resample until we find an unused logical number.
        let mut cset = ChangeSet::new();
        let mut k = [0u8; 20];
        let mut v = [0u8; 32];
        let mut op_type = OP_WRITE;
        if self.cur_round == 0 {
            op_type = OP_CREATE;
        }

        // Tracks logical key numbers in the stable range [0, entry_count_wo_del)
        // that have already been used in this changeset (by either writes or reads).
        let mut used_stable_nums: HashSet<u64> = HashSet::new();

        for _ in 0..self.wr_op_in_cset {
            let mut num = self.cur_num;
            if self.cur_round > 0 {
                num = loop {
                    let candidate = self.sp.change(self.cur_num);
                    if !used_stable_nums.contains(&candidate) {
                        used_stable_nums.insert(candidate);
                        break candidate;
                    }
                    self.cur_num += 1;
                };
            }
            let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
            //let k64 = BigEndian::read_u64(&kh[0..8]);
            //println!("AA blkcnt={:#04x} r={:#04x} op={} cur_num={:#08x} num={:#08x} k64={:#016x} k={:?} kh={:?}", self.block_count, self.cur_round, op_type, self.cur_num, num, k64, k, kh);
            self.cur_num += 1;

            cset.add_op(op_type, &kh, &k[..], &v[..], None);
        }
        if self.cur_round == 0 {
            // We use round=0 to indicate that we are populating the database

            // create entries for deletion
            for _ in 0..self.delete_op_in_cset {
                let num = self.sp.entry_count_wo_del + self.cur_delete_num;
                let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
                cset.add_op(OP_CREATE, &kh, &k[..], &v[..], None);
                self.cur_delete_num += 1;
            }
        } else {
            // We use round=1 to indicate that we are benchmarking TPS

            //// delete the created entries
            for _ in 0..self.delete_op_in_cset {
                let num = self.sp.entry_count_wo_del + self.cur_delete_num;
                let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
                if used_stable_nums.contains(&num) {
                    println!("dup in del {num}");
                }
                used_stable_nums.insert(num);
                cset.add_op(OP_DELETE, &kh, &k[..], &v[..], None);
                self.cur_delete_num = (self.cur_delete_num + PRIME2) % self.max_delete_num;
            }
            // create more entries
            for _ in 0..self.create_op_in_cset {
                let num = (self.sp.entry_count_wo_del * 2) + self.cur_create_num;
                let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
                if used_stable_nums.contains(&num) {
                    println!("dup in create {num}");
                }
                used_stable_nums.insert(num);
                cset.add_op(OP_CREATE, &kh, &k[..], &v[..], None);
                self.cur_create_num += 1;
            }
            // read more entries
            for _ in 0..self.rd_op_in_cset {
                // We draw candidates from the same stable range, but avoid
                // logical numbers already used in this changeset (by writes
                // or previous reads) to keep keys unique within the changeset.
                let num = loop {
                    let base = self.cur_read_num % self.sp.entry_count_wo_del;
                    let candidate = self.sp.change(base);
                    self.cur_read_num += 1;
                    if !used_stable_nums.contains(&candidate) {
                        used_stable_nums.insert(candidate);
                        break candidate;
                    }
                };
                let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
                cset.add_op(OP_READ, &kh, &k[..], &v[..], None);
            }
        }
        cset.sort();
        cset
    }
}
