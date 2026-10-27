#![cfg(feature = "use_nomt")]

use std::path::Path;
use std::collections::hash_map::Entry;

use parking_lot::RwLock;
use lazy_static::lazy_static;
use rayon::prelude::*;
use rayon::slice::ParallelSliceMut;

use fxhash::FxHashMap;

use nomt::{Nomt, Options, SessionParams, WitnessMode, KeyReadWrite, Session};
use nomt::hasher::Blake3Hasher;
use nomt_core::trie::KeyPath;

use qmdb_common::task::task::Task;
use qmdb::test_helper::SimpleTask;
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_WRITE, OP_READ};

// 1. Global instance

struct GlobalNomt {
    nomt: Nomt<Blake3Hasher>,
}

lazy_static! {
    static ref GLOBAL_NOMT: RwLock<Option<GlobalNomt>> = RwLock::new(None);
}

// 2. Initialization
pub fn init(db_dir: &str) {
    let path = Path::new(db_dir);
    
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("Failed to create NOMT dir");

    let mut opts = Options::new();
    opts.path(path);
    opts.commit_concurrency(8); 
    opts.io_workers(32);
    opts.metrics(true); 
    opts.prepopulate_page_cache(true);
    // Hashtable buckets per state size, from NOMT_BUCKETS: 39073369 for 100M entries,
    // 93329983 for 300M, 248354236 for 600M, 575546222 for 900M-1.5B.
    let buckets: u32 = std::env::var("NOMT_BUCKETS").ok().and_then(|s| s.parse().ok()).unwrap_or(575_546_222u32);
    println!("[NOMT] hashtable_buckets={} ({:.0}M)", buckets, buckets as f64 / 1_000_000.0);
    opts.hashtable_buckets(buckets);
    opts.page_cache_upper_levels(2);
    opts.page_cache_size(16384); // MiB
    opts.leaf_cache_size(10240); // MiB

    let nomt = Nomt::open(opts).unwrap();
    
    let mut global = GLOBAL_NOMT.write();
    *global = Some(GlobalNomt { nomt });
}

// 3. Block execution

struct TaskExecutor<'a> {
    session: &'a Session<Blake3Hasher>,
    access: FxHashMap<KeyPath, KeyReadWrite>,
}

impl<'a> TaskExecutor<'a> {
    fn new(session: &'a Session<Blake3Hasher>) -> Self {
        Self {
            session,
            access: FxHashMap::default(),
        }
    }

    fn read(&mut self, key_path: KeyPath) -> Option<Vec<u8>> {
        // 1. Read our own writes from this chunk first
        if let Some(krw) = self.access.get(&key_path) {
            return krw.last_value().map(|v| v.to_vec());
        }
        
        // 2. Otherwise read from the NOMT session
        let value = self.session.read(key_path).unwrap();
        
        // Reads are not recorded in self.access as KeyReadWrite::Read:
        // only written keys go to session.finish, which keeps
        // the access set small and cheap to sort.
        value.map(|v| v.to_vec())
    }

    fn write(&mut self, key_path: KeyPath, value: Option<&[u8]>) {
        let val_vec = value.map(|v| v.to_vec());
        match self.access.entry(key_path) {
            Entry::Occupied(mut o) => {
                o.get_mut().write(val_vec);
            }
            Entry::Vacant(v) => {
                v.insert(KeyReadWrite::Write(val_vec));
            }
        }
        // Only written keys are warmed up
        self.session.warm_up(key_path);
        self.session.preserve_prior_value(key_path);
    }
}

pub fn execute_batch(_block_height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    let global_guard = GLOBAL_NOMT.read();
    let nomt_ref = &global_guard.as_ref().expect("Nomt not initialized").nomt;

    let session_params = SessionParams::default().witness_mode(WitnessMode::disabled());
    let session = nomt_ref.begin_session(session_params);

    let thread_count = rayon::current_num_threads();
    let chunk_size = (task_list.len() + thread_count - 1) / thread_count.max(1);

    // 1. Parallel Execution
    let results: Vec<FxHashMap<KeyPath, KeyReadWrite>> = task_list
        .par_chunks(chunk_size.max(1))
        .map(|chunk| {
            let mut executor = TaskExecutor::new(&session);
            for task_lock in chunk {
                let task_opt = task_lock.read();
                if let Some(task) = task_opt.as_ref() {
                    for cset in task.get_change_sets().iter() {
                        cset.for_each_op(|op_type, _shard_id, key_hash, _key, value, _old_value, _rec| {
                            let mut h = [0u8; 32];
                            let len = key_hash.len().min(32);
                            h[0..len].copy_from_slice(&key_hash[0..len]);
                            let key_path = KeyPath::from(h);

                            match op_type {
                                OP_WRITE | OP_CREATE => executor.write(key_path, Some(value)),
                                OP_DELETE => executor.write(key_path, None),
                                OP_READ => { 
                                    // reads are not recorded
                                    let _ = executor.read(key_path); 
                                },
                                _ => {}
                            }
                        });
                    }
                }
            }
            executor.access
        })
        .collect();

    // 2. Merge the chunks' access sets in task order
    // so that a later task's write to a key overrides an earlier one
    let mut final_access: FxHashMap<KeyPath, KeyReadWrite> = FxHashMap::default();
    for chunk_access in results {
        for (k, v) in chunk_access {
            match final_access.entry(k) {
                Entry::Occupied(mut o) => {
                    if let Some(val) = v.last_value() {
                        o.get_mut().write(Some(val.to_vec()));
                    } else {
                        o.get_mut().write(None); // delete
                    }
                }
                Entry::Vacant(vac) => {
                    vac.insert(v);
                }
            }
        }
    }

    let mut actual_access: Vec<_> = final_access.into_iter().collect();

    // A block with only reads has nothing to commit
    if actual_access.is_empty() {
        return;
    }

    // 3. Sort by key path in parallel
    actual_access.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let finished = session.finish(actual_access).unwrap();
    
    finished.commit(nomt_ref).unwrap();
}

// 4. Backend interface (all writes go through execute_batch)

pub fn create_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    execute_batch(height, task_list);
}

pub fn update_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    execute_batch(height, task_list);
}

pub fn delete_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    execute_batch(height, task_list);
}

pub fn read_kv(task_list: Vec<RwLock<Option<SimpleTask>>>) {
    let global_guard = GLOBAL_NOMT.read();
    let nomt_ref = &global_guard.as_ref().expect("Nomt not initialized").nomt;

    // Reads need no chunks, TaskExecutor or access set;
    // they run in parallel with Rayon.
    task_list.par_iter().for_each(|task_lock| {
        let task_opt = task_lock.read();
        if let Some(task) = task_opt.as_ref() {
            for cset in task.get_change_sets().iter() {
                cset.for_each_op(|_op_type, _shard_id, key_hash, _key, _value, _old_value, _rec| {
                    let mut h = [0u8; 32];
                    let len = key_hash.len().min(32);
                    h[0..len].copy_from_slice(&key_hash[0..len]);
                    let key_path = KeyPath::from(h);

                    // Read from the Nomt instance directly, without a session.
                    // Nomt::read is thread-safe and synchronous,
                    // and the read is not recorded in any access set.
                    let _val = nomt_ref.read(key_path).unwrap();
                });
            }
        }
    });
}
