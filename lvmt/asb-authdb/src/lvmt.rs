use asb_options::Options;
use asb_profile::CounterTrait;
use authdb::AuthDB;
use cfx_kvdb_rocksdb::{CompactionProfile, Database, DatabaseConfig};
use kvdb::KeyValueDB;
use lvmt_db::{
    lvmt_db::{cached_pp, LvmtDB, INC_KEY_COUNT, INC_KEY_LEVEL_SUM, INC_TREE_COUNT, NUM_COLS},
    multi_layer_amt::Key,
    storage::access::PUT_COUNT,
};
use std::path::Path;
use std::sync::Arc;

// Matches adapter's MEMORY_MB constant exactly
const MEMORY_MB: usize = 2048;

pub struct Lvmt {
    amt: LvmtDB,
}

pub fn new(backend: Arc<dyn KeyValueDB>, opts: &Options) -> Lvmt {
    let pp = cached_pp("./pp");
    pp.warm_quotient();

    // Drop externally-opened backend to release RocksDB lock,
    // then reopen with adapter's exact configuration.
    drop(backend);

    let mut db_config = DatabaseConfig::with_columns(NUM_COLS);
    db_config.memory_budget = Some(MEMORY_MB);
    db_config.compaction = CompactionProfile::auto(Path::new(&opts.db_dir));
    db_config.disable_wal = false;
    db_config.enable_statistics = true;

    // Print all effective RocksDB configuration values before opening
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
        opts.db_dir,
    );

    let backend: Arc<dyn KeyValueDB> =
        Arc::new(Database::open(&db_config, &opts.db_dir).expect("Failed to open RocksDB"));

    // Same as adapter: shard_info = Some((4, 0)) when --shards 16
    let shard_info = opts.shards.map(|size| (size.trailing_zeros() as usize, 0));
    Lvmt {
        amt: LvmtDB::new(backend, pp, true, shard_info),
    }
}

impl AuthDB for Lvmt {
    fn get(&self, key: Vec<u8>) -> Option<Box<[u8]>> {
        // Matches adapter: let _ = db.get(&k)
        self.amt.get(&Key(key)).unwrap()
    }

    fn set(&mut self, key: Vec<u8>, value: Vec<u8>) {
        // Matches adapter: db.set(&k, value.to_vec().into_boxed_slice())
        self.amt.set(&Key(key), value.into_boxed_slice())
    }

    fn commit(&mut self, index: usize) {
        // Matches adapter: db.commit(height as u64).unwrap() — return value dropped
        self.amt.commit(index as u64).unwrap();
    }

    fn backend(&self) -> Option<&dyn KeyValueDB> {
        Some(&*self.amt.kvdb)
    }

    fn flush_all(&mut self) {
        self.amt.flush_root();
    }
}

#[derive(Clone)]
pub struct LvmtCounter {
    put_count: [u64; 4],
    inc_key_count: u64,
    inc_tree_count: u64,
    inc_key_level_count: u64,
}

impl Default for LvmtCounter {
    fn default() -> Self {
        Self {
            put_count: [0; 4],
            inc_key_count: 0,
            inc_tree_count: 0,
            inc_key_level_count: 0,
        }
    }
}

impl CounterTrait for LvmtCounter {
    fn report(&mut self) -> String {
        let put_count = *PUT_COUNT.lock().unwrap();
        let inc_key_count = *INC_KEY_COUNT.lock().unwrap();
        let inc_tree_count = *INC_TREE_COUNT.lock().unwrap();
        let inc_key_level_count = *INC_KEY_LEVEL_SUM.lock().unwrap();

        let key_diff = inc_key_count - self.inc_key_count;
        let tree_diff = inc_tree_count - self.inc_tree_count;
        let level_diff = inc_key_level_count - self.inc_key_level_count;
        let avg_level = (level_diff as f64) / (key_diff as f64);

        let answer = format!(
            "avg levels: {:.3}, access writes {:?}, data writes {} {}",
            avg_level,
            self.put_count
                .iter()
                .zip(put_count.iter())
                .map(|(x, y)| y - x)
                .collect::<Vec<u64>>(),
            key_diff * 2,
            tree_diff * 2,
        );

        self.put_count = *PUT_COUNT.lock().unwrap();
        self.inc_key_count = *INC_KEY_COUNT.lock().unwrap();
        self.inc_tree_count = *INC_TREE_COUNT.lock().unwrap();
        self.inc_key_level_count = *INC_KEY_LEVEL_SUM.lock().unwrap();

        answer
    }
}
