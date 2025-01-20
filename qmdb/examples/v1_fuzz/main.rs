pub mod refdb;

use crate::refdb::RefDB;
use byteorder::{ByteOrder, LittleEndian};
use hex;
use parking_lot::RwLock;
use qmdb::config::Config;
use qmdb::def::{
    DEFAULT_ENTRY_SIZE, IN_BLOCK_IDX_BITS, OP_DELETE, OP_WRITE, SENTRY_COUNT, SHARD_COUNT,
};
use qmdb::entryfile::{Entry, EntryBz};
use qmdb::tasks::TasksManager;
use qmdb::test_helper::{RandSrc, SimpleTask};
use qmdb::utils::changeset::ChangeSet;
use qmdb::utils::hasher;
use qmdb::{AdsCore, AdsWrap, ADS};
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

const BLOCK_COUNT: usize = 100; // each block about 4.5M
const BLOCK_NUM_PER_FILE: usize = 100;

#[derive(serde::Serialize, serde::Deserialize)]
struct TaskLists {
    lists: Vec<TaskList>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TaskList {
    tasks: Vec<SimpleTask>,
}

static mut ROOT_BEFORE_SET: [[u8; 32]; 32] = [[0u8; 32]; 32];

fn main() {
    // params to adjust before test
    let task_file_num: usize = 1;
    let stop_height: usize = 98;
    let mode = 0;

    if mode == 0 {
        run_fuzz_tests(task_file_num, stop_height);
    } else {
        generate_test_blocks();
    }
}

fn generate_test_block(test_gen: &mut TestGenV1, start_height: usize, end_height: usize) {
    let mut task_lists = vec![];
    for height in start_height..end_height {
        let task_list = test_gen.gen_block(height as i64);
        task_lists.push(TaskList { tasks: task_list });
    }
    let task_lists = TaskLists { lists: task_lists };
    let out = bincode::serialize(&task_lists).unwrap();
    let tasks_file = format!("tasks_for_test_{}.dat", start_height / BLOCK_NUM_PER_FILE);
    let mut file = File::create_new(tasks_file).unwrap();
    file.write_all(out.as_ref()).unwrap();
}

fn generate_test_blocks() {
    let file_name = "randsrc.dat";
    let randsrc = RandSrc::new(file_name, "qmdb-1");
    let mut test_gen = TestGenV1::new(randsrc);
    if BLOCK_COUNT < BLOCK_NUM_PER_FILE {
        generate_test_block(&mut test_gen, 1, BLOCK_COUNT + 1); // [start_height, end_height)
        return;
    }
    for i in 0..BLOCK_COUNT / BLOCK_NUM_PER_FILE {
        generate_test_block(
            &mut test_gen,
            1 + i * BLOCK_NUM_PER_FILE,
            1 + i * BLOCK_NUM_PER_FILE + BLOCK_NUM_PER_FILE,
        ); // [start_height, end_height)
    }
}

fn run_fuzz_single_round(
    test_gen: &mut TestGenV1,
    ads: &mut AdsWrap<SimpleTask>,
    task_file_sn: usize,
) -> bool /*stop?*/ {
    let tasks_file = format!("tasks_for_test_{}.dat", task_file_sn);
    let mut file = File::open(tasks_file).unwrap();
    let file_len = file.metadata().unwrap().len();
    let mut bz = vec![0u8; file_len as usize];
    file.read(&mut bz).unwrap();
    let task_lists = bincode::deserialize::<TaskLists>(&bz).unwrap().lists;
    println!(
        "task_lists len:{:?} which sn is {:?}",
        task_lists.len(),
        task_file_sn
    );

    let mut buf = [0u8; DEFAULT_ENTRY_SIZE];
    for height in 1 + task_file_sn * BLOCK_NUM_PER_FILE
        ..1 + task_file_sn * BLOCK_NUM_PER_FILE + BLOCK_NUM_PER_FILE
    {
        let task_list_raw = task_lists[height - 1 - task_file_sn * BLOCK_NUM_PER_FILE]
            .tasks
            .clone();
        let mut task_list = vec![];
        for t in task_list_raw {
            task_list.push(RwLock::new(Some(t)));
        }
        let height = height as i64;
        let task_count = task_list.len() as i64;
        println!("AA height={} task_count={:#08x}", height, task_count);
        let last_task_id = (height << IN_BLOCK_IDX_BITS) | (task_count - 1);
        let (success, _) =
            ads.start_block(height, Arc::new(TasksManager::new(task_list, last_task_id)));
        if !success {
            unsafe {
                if ROOT_BEFORE_SET[0] == [0u8; 32] {
                    for shard in 0..SHARD_COUNT {
                        let root_before = ads.get_metadb().read().get_root_hash(shard);
                        println!("root before in shard {:?}:{:?}", shard, root_before);
                        ROOT_BEFORE_SET[shard] = root_before;
                    }
                } else {
                    for shard in 0..SHARD_COUNT {
                        let root_after = ads.get_metadb().read().get_root_hash(shard);
                        println!("root after in shard {:?}:{:?}", shard, root_after);
                        //ROOT1_AFTER_SET[shard] = root_after;
                        if root_after != ROOT_BEFORE_SET[shard] {
                            panic!(
                                "root mismatch at shard {:?}, before={} after={}",
                                shard,
                                hex::encode(ROOT_BEFORE_SET[shard]),
                                hex::encode(root_after)
                            );
                        }
                    }
                }
            }
            return true;
        }
        let shared_ads = ads.get_shared();
        shared_ads.insert_extra_data(height, "".to_owned());
        for idx in 0..task_count {
            let task_id = (height << IN_BLOCK_IDX_BITS) | idx;
            println!("AA Fuzz height={} idx={:?}", height, idx);
            shared_ads.add_task(task_id);
        }
        let read_count = test_gen.get_read_count();
        println!("AA read_count={}", read_count);
        for _ in 0..read_count {
            let (k, kh, v) = test_gen.rand_read_kv(height);
            if v.len() == 0 {
                continue;
            }
            let (size, ok) = shared_ads.read_entry(height, &kh[..], &k[..], &mut buf);
            if !ok {
                panic!("Cannot read entry");
            }
            if buf[..size] != v[..] {
                let tmp1 = EntryBz { bz: &v[..] };
                let r = Entry::from_bz(&tmp1);
                let tmp2 = EntryBz { bz: &buf[..size] };
                let i = Entry::from_bz(&tmp2);
                println!("R {:?}", r);
                println!("I {:?}", i);
                panic!(
                    "Value mismatch k={} ref_v={} imp_v={}",
                    hex::encode(k),
                    hex::encode(v),
                    hex::encode(&buf[..size]),
                );
            }
        }
    }
    return false;
}

fn run_fuzz_tests(task_file_num: usize, stop_height: usize) {
    let file_name = "randsrc.dat";
    let randsrc = RandSrc::new(file_name, "qmdb-1");
    let mut test_gen = TestGenV1::new(randsrc);
    let wrbuf_size = 256 * 1024;
    let file_segment_size = 128 * 1024 * 1024;
    for i in 0..2 {
        let ads_dir = format!("ADS{}", i);
        let ads_dir = ads_dir.as_str();
        let mut config = Config::new(
            ads_dir,
            wrbuf_size,
            file_segment_size,
            true,
            None,
            7000,
            7,
            10,
            200000,
            128,
            32,
            1024,
            20000,
        );
        if cfg!(feature = "tee_cipher") {
            config.set_aes_keys([1u8; 96]);
        }

        AdsCore::init_dir(&config);
        let mut ads: AdsWrap<_> = AdsWrap::new(&config);
        ads.set_stop_block(stop_height as i64);
        for n in 0..task_file_num {
            let is_stop = run_fuzz_single_round(&mut test_gen, &mut ads, n);
            if is_stop {
                break;
            }
        }
        //remove_dir_all(ads_dir);
    }
}

fn rand_between(randsrc: &mut RandSrc, min: usize, max: usize) -> usize {
    let span = max - min;
    min + randsrc.get_uint32() as usize % span
}

fn hash(n: usize) -> [u8; 32] {
    let mut bz = [0u8; 32];
    LittleEndian::write_u64(&mut bz[0..8], n as u64);
    hasher::hash(&bz[..])
}

fn rand_hash(randsrc: &mut RandSrc) -> [u8; 32] {
    let mut bz = [0u8; 32];
    LittleEndian::write_u64(&mut bz[0..8], randsrc.get_uint64());
    hasher::hash(&bz[..])
}

pub struct TestGenV1 {
    pub change_set_size_min: usize,
    pub change_set_size_max: usize,
    pub block_size_min: usize,
    pub block_size_max: usize,
    pub key_count_max: usize,
    pub active_num_to_start_remove: usize,
    pub active_num_to_start_read: usize,
    pub remove_prob: usize,
    pub max_cset_in_task: usize,
    pub max_read_count: usize,
    pub randsrc: RandSrc,
    refdb: RefDB,
}

impl TestGenV1 {
    pub fn new(randsrc: RandSrc) -> Self {
        let dir = "TestGenV1";
        if Path::new(dir).exists() {
            fs::remove_dir_all(dir).unwrap();
        }
        let config = Config::from_dir(dir);
        let refdb = RefDB::new("refdb", &config);

        let total_sentry = SENTRY_COUNT * SHARD_COUNT;
        Self {
            change_set_size_min: 5,
            change_set_size_max: 50,
            block_size_min: 10,
            block_size_max: 500,
            key_count_max: 3 << 16,
            active_num_to_start_remove: total_sentry + (3 << 14),
            active_num_to_start_read: total_sentry + (3 << 14),
            remove_prob: 20, // 20%
            max_cset_in_task: 5,
            max_read_count: 20,
            randsrc,
            refdb,
        }
    }

    pub fn gen_block(&mut self, height: i64) -> Vec<SimpleTask> {
        let blk_size = rand_between(&mut self.randsrc, self.block_size_min, self.block_size_max);
        let mut res = Vec::with_capacity(blk_size);
        for i in 0..blk_size {
            let _task_id = (height << IN_BLOCK_IDX_BITS) | (i as i64);
            let count = rand_between(&mut self.randsrc, 1, self.max_cset_in_task);
            //println!("BB gen_block task_id={:#08x} count={}", task_id, count);
            let task = self.gen_task(count);
            res.push(task);
        }
        self.refdb.end_block();
        res
    }

    fn gen_task(&mut self, count: usize) -> SimpleTask {
        let mut v = Vec::with_capacity(count);
        for _ in 0..count {
            v.push(self.gen_cset());
            self.refdb.end_tx();
        }
        SimpleTask::new(v)
    }

    fn gen_cset(&mut self) -> ChangeSet {
        let mut pre_cset = ChangeSet::new();
        let cset_size = rand_between(
            &mut self.randsrc,
            self.change_set_size_min,
            self.change_set_size_max,
        );
        let mut keys = HashSet::new();
        for _ in 0..cset_size {
            self.gen_op(&mut pre_cset, &mut keys);
        }
        pre_cset.sort();
        let mut cset = ChangeSet::new();
        // drive refdb using pre_cset
        pre_cset.apply_op_in_range(|op_type, _kh, k, v, _, _rec| {
            if op_type == OP_WRITE {
                let rec = self.refdb.set_entry(k, v);
                cset.add_op_rec(rec);
            } else {
                if let Some(rec) = self.refdb.remove_entry(k) {
                    cset.add_op_rec(rec);
                }
            }
        });
        // cset will not get its order changed because pre_cset was already sorted
        cset.sort();
        cset
    }

    fn gen_op(&mut self, cset: &mut ChangeSet, keys: &mut HashSet<usize>) {
        let active_num = self.refdb.total_num_active();
        let mut k_num = rand_between(&mut self.randsrc, 0, self.key_count_max);
        // a ChangeSet cannot contain duplicated keys
        while keys.contains(&k_num) {
            k_num = rand_between(&mut self.randsrc, 0, self.key_count_max);
        }
        keys.insert(k_num);
        let k = hash(k_num);
        let kh = hasher::hash(k);
        let v = rand_hash(&mut self.randsrc);
        let mut op = OP_WRITE;
        if active_num > self.active_num_to_start_remove
            && rand_between(&mut self.randsrc, 0, 100) > self.remove_prob
        {
            op = OP_DELETE;
        }
        cset.add_op(op, kh[0] >> 4, &kh, &k[..], &v[..], None);
    }

    fn get_read_count(&mut self) -> usize {
        let active_num = self.refdb.total_num_active();
        if active_num < self.active_num_to_start_read {
            return 0;
        }
        rand_between(&mut self.randsrc, 0, self.max_read_count)
    }

    //fn read_entry(&self, key_hash: &[u8], key: &[u8], buf: &mut [u8]) -> (usize, bool)
    fn rand_read_kv(&mut self, curr_height: i64) -> ([u8; 32], [u8; 32], Vec<u8>) {
        let mut count = 0usize;
        loop {
            let k_num = rand_between(&mut self.randsrc, 0, self.key_count_max);
            let k = hash(k_num);
            let kh = hasher::hash(&k[..]);
            let v_opt = self.refdb.get_entry(&kh);
            if v_opt.is_none() {
                //println!("AA try rand_read missed k_num={}", k_num);
                continue; //retry till hit
            }
            //println!("AA try rand_read hit k_num={}", k_num);
            let v = v_opt.unwrap();
            let e = EntryBz { bz: &v[..] };
            let create_height = e.version() >> IN_BLOCK_IDX_BITS;
            if create_height + 2 > curr_height {
                count += 1;
                if count > 10 {
                    return (k, kh, vec![]);
                }
                continue; //retry to avoid recent
            }
            return (k, kh, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use crate::main;

    #[test]
    #[serial]
    fn run_v1_fuzz() {
        std::panic::catch_unwind(|| main());
    }
}
