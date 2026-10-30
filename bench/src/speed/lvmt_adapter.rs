#![cfg(feature = "use_lvmt")]

use lazy_static::lazy_static;
use parking_lot::RwLock;
use std::path::Path;
use std::sync::{Arc, Mutex};

use qmdb::test_helper::SimpleTask;
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE};
use qmdb_common::task::task::Task;

use lvmt_db::{
    lvmt_db::{cached_pp, LvmtDB, NUM_COLS},
    multi_layer_amt::Key,
};

use cfx_kvdb_rocksdb::{CompactionProfile, Database, DatabaseConfig};

// Single-shard mode matching LVMT's own benchmark with --shards 16.
//
// LVMT's design (from the paper): each full node only maintains proof
// generation metadata for ONE shard (e.g., keys sharing the same 4-bit
// prefix). The state root (commitment) is maintained by all nodes.
//
// shard_info = Some((4, 0)):
//   - All keys written to one LvmtDB.
//   - Only keys where key_hash[0]>>4 == 0 (1/16 of the key space) are
//     "in_proof_shard" → full AMT inner-node maintenance (O(log n) I/O).
//   - The other 15/16 only update the commitment (O(1), no I/O amplification).
//
// This exactly matches LVMT's --shards 16 benchmark behavior and represents
// the per-node workload in a 16-node deployment.
// SHARD_LOG2=0 means no proof sharding: this node maintains full AMT proof
// metadata for all keys (equivalent to --shards 1 in LVMT's own benchmark).
const SHARD_LOG2: usize = 4;
// 32 GB cgroup cap. NUM_COLS=3 makes write_buffer × 3 CFs × 2 versions a big
// multiplier. 24 GB and 16 GB MEMORY_MB both OOM'd at 33 GB anon-rss.
// With NUM_COLS=3:
//   block_cache       = M/3
//   write_buffer total = M  (i.e., 3 × M/6 × 2)
//   declared total    = 4M/3
// + LVMT BLS prover + Rust runtime + RocksDB compaction temp ~7 GB
// 8 GB declared → ~11 GB RocksDB + ~7 GB overhead = ~18 GB physical
//   → 14 GB margin under 32 GB cap, comfortable.
const MEMORY_MB: usize = 2048;

struct LvmtShard(LvmtDB);
unsafe impl Send for LvmtShard {}
unsafe impl Sync for LvmtShard {}

struct GlobalLvmt {
    db: LvmtShard,
}

lazy_static! {
    static ref GLOBAL_LVMT: Mutex<Option<GlobalLvmt>> = Mutex::new(None);
}

pub fn init(db_dir: &str) {
    let _ = std::fs::remove_dir_all(db_dir);
    std::fs::create_dir_all(db_dir).expect("Failed to create db dir");

    let pp = cached_pp("../lvmt/pp");
    pp.warm_quotient();

    let mut db_config = DatabaseConfig::with_columns(NUM_COLS);
    db_config.memory_budget = Some(MEMORY_MB);
    db_config.compaction = CompactionProfile::auto(Path::new(db_dir));
    db_config.disable_wal = false;          // WAL on (matches QMDB hpfile durability)
    db_config.enable_statistics = false;    // turn off — adds CPU overhead during bench
    db_config.max_open_files = -1;          // 1.5B-key DB has many SSTs; avoid open/close churn

    let mem_total_mb = db_config.memory_budget.unwrap_or(128);
    let mem_per_col_mb = mem_total_mb / NUM_COLS as usize;
    println!("[RocksDB config] columns={} memory_budget={}MiB memory_per_col={}MiB \
              block_cache={}MiB write_buffer_per_col={}MiB \
              compaction.initial_file_size={}MiB compaction.block_size={}KB \
              compaction.write_rate_limit={:?} \
              max_open_files={} disable_wal={} enable_statistics={} \
              db_dir={}",
        NUM_COLS,
        mem_total_mb,
        mem_per_col_mb,
        mem_total_mb / 3,
        mem_per_col_mb / 2,
        db_config.compaction.initial_file_size / 1024 / 1024,
        db_config.compaction.block_size / 1024,
        db_config.compaction.write_rate_limit,
        db_config.max_open_files,
        db_config.disable_wal,
        db_config.enable_statistics,
        db_dir,
    );

    let backend: Arc<dyn kvdb::KeyValueDB> =
        Arc::new(Database::open(&db_config, db_dir).expect("Failed to open RocksDB"));

    let db = LvmtDB::new(backend, pp, true, Some((SHARD_LOG2, 0)));

    *GLOBAL_LVMT.lock().unwrap() = Some(GlobalLvmt { db: LvmtShard(db) });
}

fn execute_writes(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    // Collect all ops across the entire block, sort by key (matches LVMT official
    // benchmark: ~/lvmt/benchmarks/src/run.rs warmup() sorts before db.set).
    // Sorted order improves RocksDB SST locality during compaction.
    let mut all_ops: Vec<(Vec<u8>, u8, Vec<u8>)> = Vec::with_capacity(100_000);
    for task_lock in &task_list {
        let task_opt = task_lock.read();
        if let Some(task) = task_opt.as_ref() {
            for cset in task.get_change_sets().iter() {
                cset.for_each_op(
                    |op_type, _shard_id, key_hash: &[u8; 32], _key, value: &[u8], _old_value, _rec| {
                        all_ops.push((key_hash.to_vec(), op_type, value.to_vec()));
                    },
                );
            }
        }
    }
    all_ops.sort_by(|(k1, _, _), (k2, _, _)| k1.cmp(k2));

    let mut global = GLOBAL_LVMT.lock().unwrap();
    let g = global.as_mut().expect("LVMT not initialized");
    let db = &mut g.db.0;

    // LVMT's commit() assumes each key is written at most once per epoch (it panics in
    // Key::mid once a key is written ~86 times), so keep only the last write of each
    // key. Equal keys are adjacent after the stable sort. Reads are unaffected: get()
    // never sees uncommitted writes.
    let mut superseded = vec![false; all_ops.len()];
    let mut later_write: Option<&Vec<u8>> = None;
    for (i, (kh, op_type, _)) in all_ops.iter().enumerate().rev() {
        if matches!(*op_type, OP_CREATE | OP_WRITE | OP_DELETE) {
            superseded[i] = later_write == Some(kh);
            later_write = Some(kh);
        }
    }

    for (i, (kh, op_type, value)) in all_ops.iter().enumerate() {
        let k = Key(kh.clone());
        match *op_type {
            OP_CREATE | OP_WRITE if !superseded[i] => {
                db.set(&k, value.clone().into_boxed_slice());
            }
            OP_DELETE if !superseded[i] => {
                db.set(&k, Box::new([]));
            }
            OP_READ => {
                let _ = db.get(&k);
            }
            _ => {}
        }
    }

    db.commit(height as u64).unwrap();
}

// Public interface

pub fn create_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    execute_writes(height, task_list);
}

pub fn update_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    execute_writes(height, task_list);
}

pub fn delete_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    execute_writes(height, task_list);
}

pub fn read_kv(task_list: Vec<RwLock<Option<SimpleTask>>>) {
    let global = GLOBAL_LVMT.lock().unwrap();
    let g = global.as_ref().expect("LVMT not initialized");
    let db = &g.db.0;

    for task_lock in &task_list {
        let task_opt = task_lock.read();
        if let Some(task) = task_opt.as_ref() {
            for cset in task.get_change_sets().iter() {
                cset.for_each_op(
                    |_op_type, _shard_id, key_hash: &[u8; 32], _key, _value: &[u8], _old_value, _rec| {
                        let k = Key(key_hash.to_vec());
                        let _ = db.get(&k);
                    },
                );
            }
        }
    }
}
