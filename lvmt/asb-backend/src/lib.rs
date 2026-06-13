use asb_options::{Backend, Options};
use kvdb::KeyValueDB;
use std::sync::Arc;

#[cfg(not(feature = "parity-backend"))]
mod cfx_kvdb_rocksdb;

#[cfg(not(feature = "parity-backend"))]
mod db_with_mertics;

#[cfg(feature = "lmpts-backend")]
pub extern crate cfx_storage;

#[cfg(all(feature = "parity-backend", feature = "lmpts-backend"))]
compile_error!("Multiple backends are chosen!");

mod in_mem_with_metrics;
mod mdbx;

#[cfg(feature = "parity-backend")]
mod parity_kvdb_rocksdb;

pub fn backend(opts: &Options) -> Arc<dyn KeyValueDB> {
    match opts.backend {
        Backend::RocksDB => {
            let db_dir = opts.db_dir.as_str();
            #[cfg(not(feature = "parity-backend"))]
            {
                cfx_kvdb_rocksdb::open(db_dir, opts)
            }
            #[cfg(feature = "parity-backend")]
            {
                parity_kvdb_rocksdb::open(db_dir, opts.num_cols())
            }
        }
        Backend::InMemoryDB => Arc::new(kvdb_memorydb::create(opts.num_cols())),
        Backend::MDBX => Arc::new(mdbx::open_database(opts)),
    }
}
