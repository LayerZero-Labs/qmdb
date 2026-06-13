#![allow(unused)]

use std::io::Read;
use std::sync::Arc;

use cfx_kvdb_rocksdb::Database;
use kvdb::{DBOp, DBTransaction, DBValue, KeyValueDB};
use parity_util_mem::{MallocSizeOf, MallocSizeOfOps};

// Database with enabled statistics
pub struct DatabaseWithMetrics {
    db: Arc<Database>,
    pub reads: std::sync::atomic::AtomicI64,
    pub writes: std::sync::atomic::AtomicI64,
    bytes_read: std::sync::atomic::AtomicI64,
    bytes_written: std::sync::atomic::AtomicI64,
}

impl DatabaseWithMetrics {
    /// Create a new instance
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            reads: std::sync::atomic::AtomicI64::new(0),
            writes: std::sync::atomic::AtomicI64::new(0),
            bytes_read: std::sync::atomic::AtomicI64::new(0),
            bytes_written: std::sync::atomic::AtomicI64::new(0),
        }
    }
}

impl MallocSizeOf for DatabaseWithMetrics {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        MallocSizeOf::size_of(&*self.db, ops)
    }
}

impl KeyValueDB for DatabaseWithMetrics {
    fn get(&self, col: u32, key: &[u8]) -> std::io::Result<Option<DBValue>> {
        let res = self.db.get(col, key);
        let count = res
            .as_ref()
            .map_or(0, |y| y.as_ref().map_or(0, |x| x.bytes().count()));

        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.bytes_read
            .fetch_add(count as i64, std::sync::atomic::Ordering::Relaxed);

        res
    }

    fn get_by_prefix(&self, col: u32, prefix: &[u8]) -> Option<Box<[u8]>> {
        let res = self.db.get_by_prefix(col, prefix);
        let count = res.as_ref().map_or(0, |x| x.bytes().count());

        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.bytes_read
            .fetch_add(count as i64, std::sync::atomic::Ordering::Relaxed);

        res
    }
    fn write_buffered(&self, transaction: DBTransaction) {
        let mut count = 0;
        for op in &transaction.ops {
            count += match op {
                DBOp::Insert { value, .. } => value.bytes().count(),
                _ => 0,
            };
        }

        self.writes.fetch_add(
            transaction.ops.len() as i64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.bytes_written
            .fetch_add(count as i64, std::sync::atomic::Ordering::Relaxed);

        self.db.write_buffered(transaction)
    }
    fn write(&self, transaction: DBTransaction) -> std::io::Result<()> {
        let mut count = 0;
        for op in &transaction.ops {
            count += match op {
                DBOp::Insert { value, .. } => value.bytes().count(),
                _ => 0,
            };
        }

        self.bytes_written
            .fetch_add(count as i64, std::sync::atomic::Ordering::Relaxed);
        self.writes.fetch_add(
            transaction.ops.len() as i64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.db.write(transaction)
    }
    fn flush(&self) -> std::io::Result<()> {
        self.db.flush()
    }

    fn iter<'a>(&'a self, col: u32) -> Box<(dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a)> {
        KeyValueDB::iter(&*self.db, col)
    }

    fn iter_from_prefix<'a>(
        &'a self,
        col: u32,
        prefix: &'a [u8],
    ) -> Box<dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a> {
        self.db.iter_from_prefix(col, prefix)
    }

    fn restore(&self, new_db: &str) -> std::io::Result<()> {
        self.db.restore(new_db)
    }
}

impl stats::PrometheusMetrics for DatabaseWithMetrics {
    fn prometheus_metrics(&self, p: &mut stats::PrometheusRegistry) {
        p.register_counter(
            "kvdb_reads",
            "db reads",
            self.reads.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        p.register_counter(
            "kvdb_writes",
            "db writes",
            self.writes.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        p.register_counter(
            "kvdb_bytes_read",
            "db bytes_reads",
            self.bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        p.register_counter(
            "kvdb_bytes_written",
            "db bytes_written",
            self.bytes_written
                .load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
    }
}
