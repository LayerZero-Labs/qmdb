use std::path::Path;
use std::sync::Arc;

use cfx_kvdb_rocksdb::{CompactionProfile, Database, DatabaseConfig};

use asb_options::Options;

pub fn open(db_dir: &str, opts: &Options) -> Arc<Database> {
    let mut db_config = DatabaseConfig::with_columns(opts.num_cols());

    db_config.memory_budget = Some(opts.cache_size as usize);
    db_config.compaction = CompactionProfile::auto(Path::new(db_dir));
    db_config.disable_wal = false;
    #[cfg(not(any(feature = "parity-backend", feature = "lmpts-backend")))]
    {
        db_config.enable_statistics = !opts.no_stat;
    }

    let db = Database::open(&db_config, db_dir).unwrap();

    Arc::new(db)
}
