// The original journaldb relies on some metric tools in crate `ethcore-db`. But it doesn't rely on the other dependencies.

/// Ethcore definition of a KeyValueDB with embeeded metrics
// pub trait KeyValueDB: kvdb::KeyValueDB + stats::PrometheusMetrics {}

#[cfg(test)]
pub use self::memory_db::InMemoryWithMetrics;

#[cfg(test)]
mod memory_db {
    // Copyright 2015-2020 Parity Technologies (UK) Ltd.
    // The following code is part of OpenEthereum.

    use parity_util_mem05::{MallocSizeOf, MallocSizeOfOps};

    // OpenEthereum is free software: you can redistribute it and/or modify
    // it under the terms of the GNU General Public License as published by
    // the Free Software Foundation, either version 3 of the License, or
    // (at your option) any later version.

    // OpenEthereum is distributed in the hope that it will be useful,
    // but WITHOUT ANY WARRANTY; without even the implied warranty of
    // MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    // GNU General Public License for more details.

    // You should have received a copy of the GNU General Public License
    // along with OpenEthereum.  If not, see <http://www.gnu.org/licenses/>.

    /// InMemory with disabled statistics
    pub struct InMemoryWithMetrics {
        db: kvdb_memorydb::InMemory,
    }

    impl MallocSizeOf for InMemoryWithMetrics {
        fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
            parity_util_mem05::MallocSizeOf::size_of(&self.db, ops)
        }
    }

    impl kvdb::KeyValueDB for InMemoryWithMetrics {
        fn get(&self, col: u32, key: &[u8]) -> std::io::Result<Option<kvdb::DBValue>> {
            self.db.get(col, key)
        }
        fn get_by_prefix(&self, col: u32, prefix: &[u8]) -> Option<Box<[u8]>> {
            self.db.get_by_prefix(col, prefix)
        }
        fn write_buffered(&self, transaction: kvdb::DBTransaction) {
            self.db.write_buffered(transaction)
        }
        fn write(&self, transaction: kvdb::DBTransaction) -> std::io::Result<()> {
            self.db.write(transaction)
        }
        fn flush(&self) -> std::io::Result<()> {
            self.db.flush()
        }

        fn iter<'a>(&'a self, col: u32) -> Box<(dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a)> {
            kvdb::KeyValueDB::iter(&self.db, col)
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

    impl stats::PrometheusMetrics for InMemoryWithMetrics {
        fn prometheus_metrics(&self, _: &mut stats::PrometheusRegistry) {}
    }

    impl InMemoryWithMetrics {
        /// Create new instance
        pub fn create(num_cols: u32) -> Self {
            Self {
                db: kvdb_memorydb::create(num_cols),
            }
        }
    }
}
