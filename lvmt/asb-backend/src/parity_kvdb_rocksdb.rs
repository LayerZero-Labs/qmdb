use kvdb::{DBOp, DBTransaction, DBValue, IoStats, IoStatsKind, KeyValueDB};
use kvdb07::{
    DBOp as DBOp07, DBTransaction as DBTransaction07, IoStatsKind as IoStatsKind07,
    KeyValueDB as KeyValueDB07,
};
use kvdb_rocksdb::{CompactionProfile, Database, DatabaseConfig};
use parity_util_mem::{MallocSizeOf, MallocSizeOfOps};
use std::path::Path;
use std::sync::{Arc, RwLock};

pub fn open(db_dir: &str, num_cols: u32) -> Arc<dyn KeyValueDB> {
    let mut config = DatabaseConfig::with_columns(num_cols);
    config.enable_statistics = false;
    config.compaction = CompactionProfile::auto(&Path::new(db_dir));

    let db: WrappedDataBase = kvdb_rocksdb::Database::open(&config, db_dir)
        .unwrap()
        .into();
    Arc::new(db)
}

pub struct WrappedDataBase {
    pub db: Database,
    pub buffered_transactions: RwLock<Vec<DBOp07>>,
}

impl MallocSizeOf for WrappedDataBase {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        unimplemented!()
    }
}

impl From<Database> for WrappedDataBase {
    fn from(db: Database) -> Self {
        Self {
            db,
            buffered_transactions: Default::default(),
        }
    }
}

impl KeyValueDB for WrappedDataBase {
    fn get(&self, col: u32, key: &[u8]) -> std::io::Result<Option<DBValue>> {
        KeyValueDB07::get(&self.db, col, key)
    }

    fn get_by_prefix(&self, col: u32, prefix: &[u8]) -> Option<Box<[u8]>> {
        KeyValueDB07::get_by_prefix(&self.db, col, prefix)
    }

    fn write_buffered(&self, mut transaction: DBTransaction) {
        let ops: Vec<DBOp07> = transaction
            .ops
            .drain(..)
            .map(|x| match x {
                DBOp::Insert { col, key, value } => DBOp07::Insert { col, key, value },
                DBOp::Delete { col, key } => DBOp07::Delete { col, key },
            })
            .collect();
        let txs = &mut *self.buffered_transactions.write().unwrap();
        txs.extend_from_slice(&ops);
    }

    fn flush(&self) -> std::io::Result<()> {
        let ops = std::mem::take(&mut *self.buffered_transactions.write().unwrap());
        KeyValueDB07::write(&self.db, DBTransaction07 { ops })
    }

    fn iter<'a>(&'a self, col: u32) -> Box<dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a> {
        KeyValueDB07::iter(&self.db, col)
    }

    fn iter_from_prefix<'a>(
        &'a self,
        col: u32,
        prefix: &'a [u8],
    ) -> Box<dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a> {
        KeyValueDB07::iter_with_prefix(&self.db, col, prefix)
    }

    fn restore(&self, new_db: &str) -> std::io::Result<()> {
        KeyValueDB07::restore(&self.db, new_db)
    }

    fn io_stats(&self, kind: IoStatsKind) -> IoStats {
        let kind = match kind {
            IoStatsKind::Overall => IoStatsKind07::Overall,
            IoStatsKind::SincePrevious => IoStatsKind07::SincePrevious,
        };
        let stats = KeyValueDB07::io_stats(&self.db, kind);
        IoStats {
            transactions: stats.transactions,
            reads: stats.reads,
            cache_reads: stats.cache_reads,
            writes: stats.writes,
            bytes_read: stats.bytes_read,
            cache_read_bytes: stats.cache_read_bytes,
            bytes_written: stats.bytes_written,
            started: stats.started,
            span: stats.span,
        }
    }
}
