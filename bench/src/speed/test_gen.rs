use byteorder::{BigEndian, ByteOrder};
use parking_lot::RwLock;
use qmdb::{
    def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE},
    test_helper::{RandSrc, SimpleTask},
    utils::{byte0_to_shard_id, changeset::ChangeSet, hasher},
};

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
    cur_read_num: u64,
    cur_delete_num: u64,
    max_delete_num: u64,
    cur_create_num: u64,
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

    fn skip_tasks(&mut self, n: u64) {
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
        let hash = hasher::hash(&k[12..20]);
        k[20..20 + 32].copy_from_slice(&hash[..]);
        for i in 0..12 {
            // fill zero bytes with pseudo-random values
            k[i] = k[20 + i] ^ k[32 + i];
        }
        let kh = hasher::hash(&k[..]);

        BigEndian::write_u32(&mut v[..4], self.cur_round as u32);
        v[4..].copy_from_slice(&kh[4..]);
        kh
    }

    fn gen_cset(&mut self) -> ChangeSet {
        let mut cset = ChangeSet::new();
        let mut k = [0u8; 32 + 20];
        let mut v = [0u8; 32];
        let mut op_type = OP_WRITE;
        if self.cur_round == 0 {
            op_type = OP_CREATE;
        }

        // When we are populating the database, we do 9 creates + 1 delete
        // When we are benchmarking TPS, we do 9 updates + 1 delete + 1 create + 15 reads

        for _ in 0..self.wr_op_in_cset {
            let mut num = self.cur_num;
            if self.cur_round > 0 {
                num = self.sp.change(self.cur_num);
            }
            let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
            let shard_id = byte0_to_shard_id(kh[0]) as u8;
            //let k64 = BigEndian::read_u64(&kh[0..8]);
            //println!("AA blkcnt={:#04x} r={:#04x} op={} cur_num={:#08x} num={:#08x} k64={:#016x} k={:?} kh={:?}", self.block_count, self.cur_round, op_type, self.cur_num, num, k64, k, kh);
            self.cur_num += 1;

            cset.add_op(op_type, shard_id, &kh, &k[..], &v[..], None);
        }
        if self.cur_round == 0 {
            // We use round=0 to indicate that we are populating the database

            // create entries for deletion
            for _ in 0..self.delete_op_in_cset {
                let num = self.sp.entry_count_wo_del + self.cur_delete_num;
                let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
                let shard_id = byte0_to_shard_id(kh[0]) as u8;
                cset.add_op(OP_CREATE, shard_id, &kh, &k[..], &v[..], None);
                self.cur_delete_num += 1;
            }
        } else {
            // We use round=1 to indicate that we are benchmarking TPS

            // delete the created entries
            for _ in 0..self.delete_op_in_cset {
                let num = self.sp.entry_count_wo_del + self.cur_delete_num;
                let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
                let shard_id = byte0_to_shard_id(kh[0]) as u8;
                cset.add_op(OP_DELETE, shard_id, &kh, &k[..], &v[..], None);
                self.cur_delete_num = (self.cur_delete_num + PRIME2) % self.max_delete_num;
            }
            //create more entries
            for _ in 0..self.create_op_in_cset {
                let num = (self.sp.entry_count_wo_del * 2) + self.cur_create_num;
                let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
                let shard_id = byte0_to_shard_id(kh[0]) as u8;
                cset.add_op(OP_CREATE, shard_id, &kh, &k[..], &v[..], None);
                self.cur_create_num += 1;
            }
            //read more entries
            for _ in 0..self.rd_op_in_cset {
                let num = self.cur_read_num % self.sp.entry_count_wo_del;
                let num = self.sp.change(num);
                let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
                let shard_id = byte0_to_shard_id(kh[0]) as u8;
                cset.add_op(OP_READ, shard_id, &kh, &k[..], &v[..], None);
                self.cur_read_num += 1;
            }
        }
        cset.sort();
        cset
    }
}
