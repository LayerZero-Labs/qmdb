// Copyright 2015-2020 Parity Technologies (UK) Ltd.
// This file is part of OpenEthereum.

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

//! Disk-backed `HashDB` implementation.

use std::{
    collections::{hash_map::Entry, BTreeMap, HashMap},
    io,
    sync::Arc,
};

use super::{
    error_key_already_exists, error_negatively_reference_hash, memory_db::*, LATEST_ERA_KEY,
};
use crate::hasher::DBHasher;
use crate::KeyValueDB;
use bytes::Bytes;
use ethereum_types::H256;
use hash_db::HashDB;
use kvdb::{DBTransaction, DBValue};
use rlp::{decode, encode};
use traits::JournalDB;
use DB_PREFIX_LEN;

/// Implementation of the `HashDB` trait for a disk-backed database with a memory overlay
/// and latent-removal semantics.
///
/// Like `OverlayDB`, there is a memory overlay; `commit()` must be called in order to
/// write operations out to disk. Unlike `OverlayDB`, `remove()` operations do not take effect
/// immediately. As this is an "archive" database, nothing is ever removed. This means
/// that the states of any block the node has ever processed will be accessible.
pub struct ArchiveDB {
    overlay: MemoryDB<DBHasher, DBValue>,
    backing: Arc<dyn KeyValueDB>,
    latest_era: Option<u64>,
    column: u32,
}

impl ArchiveDB {
    /// Create a new instance from a key-value db.
    pub fn new(backing: Arc<dyn KeyValueDB>, column: u32) -> ArchiveDB {
        let latest_era = backing
            .get(column, &LATEST_ERA_KEY)
            .expect("Low-level database error.")
            .map(|val| decode::<u64>(&val).expect("decoding db value failed"));
        ArchiveDB {
            overlay: ::new_memory_db(),
            backing,
            latest_era,
            column,
        }
    }

    fn payload(&self, key: &H256) -> Option<DBValue> {
        self.backing
            .get(self.column, key.as_bytes())
            .expect("Low-level database error. Some issue with your hard disk?")
    }
}

impl HashDB<DBHasher, DBValue> for ArchiveDB {
    fn get(&self, key: &H256) -> Option<DBValue> {
        if let Some((d, rc)) = self.overlay.raw(key) {
            if rc > 0 {
                return Some(d.clone());
            }
        }
        self.payload(key)
    }

    fn contains(&self, key: &H256) -> bool {
        HashDB::<DBHasher, DBValue>::get(self, key).is_some()
    }

    fn insert(&mut self, value: &[u8]) -> H256 {
        self.overlay.insert(value)
    }

    fn emplace(&mut self, key: H256, value: DBValue) {
        self.overlay.emplace(key, value);
    }

    fn remove(&mut self, key: &H256) {
        self.overlay.remove(key);
    }
}

impl ::traits::KeyedHashDB for ArchiveDB {
    fn keys(&self) -> HashMap<H256, i32> {
        let mut ret: HashMap<H256, i32> = self
            .backing
            .iter(self.column)
            .map(|(key, _)| (H256::from_slice(&*key), 1))
            .collect();

        for (key, refs) in self.overlay.keys() {
            match ret.entry(key) {
                Entry::Occupied(mut entry) => {
                    *entry.get_mut() += refs;
                }
                Entry::Vacant(entry) => {
                    entry.insert(refs);
                }
            }
        }
        ret
    }
}

impl JournalDB for ArchiveDB {
    fn boxed_clone(&self) -> Box<dyn JournalDB> {
        Box::new(ArchiveDB {
            overlay: self.overlay.clone(),
            backing: self.backing.clone(),
            latest_era: self.latest_era,
            column: self.column.clone(),
        })
    }

    fn get_sizes(&self, sizes: &mut BTreeMap<String, usize>) {
        sizes.insert(String::from("db_archive_overlay"), self.overlay.len());
    }

    fn is_empty(&self) -> bool {
        self.latest_era.is_none()
    }

    fn journal_under(
        &mut self,
        batch: &mut DBTransaction,
        now: u64,
        _id: &H256,
    ) -> io::Result<u32> {
        let mut inserts = 0usize;
        let mut deletes = 0usize;

        for i in self.overlay.drain() {
            let (key, (value, rc)) = i;
            if rc > 0 {
                batch.put(self.column, key.as_bytes(), &value);
                inserts += 1;
            }
            if rc < 0 {
                assert!(rc == -1);
                deletes += 1;
            }
        }

        if self.latest_era.map_or(true, |e| now > e) {
            batch.put(self.column, &LATEST_ERA_KEY, &encode(&now));
            self.latest_era = Some(now);
        }
        Ok((inserts + deletes) as u32)
    }

    fn mark_canonical(
        &mut self,
        _batch: &mut DBTransaction,
        _end_era: u64,
        _canon_id: &H256,
    ) -> io::Result<u32> {
        // keep everything! it's an archive, after all.
        Ok(0)
    }

    fn inject(&mut self, batch: &mut DBTransaction) -> io::Result<u32> {
        let mut inserts = 0usize;
        let mut deletes = 0usize;

        for i in self.overlay.drain() {
            let (key, (value, rc)) = i;
            if rc > 0 {
                if self.backing.get(self.column, key.as_bytes())?.is_some() {
                    return Err(error_key_already_exists(&key));
                }
                batch.put(self.column, key.as_bytes(), &value);
                inserts += 1;
            }
            if rc < 0 {
                assert!(rc == -1);
                if self.backing.get(self.column, key.as_bytes())?.is_none() {
                    return Err(error_negatively_reference_hash(&key));
                }
                batch.delete(self.column, key.as_bytes());
                deletes += 1;
            }
        }

        Ok((inserts + deletes) as u32)
    }

    fn latest_era(&self) -> Option<u64> {
        self.latest_era
    }

    fn is_pruned(&self) -> bool {
        false
    }

    fn backing(&self) -> &Arc<dyn KeyValueDB> {
        &self.backing
    }

    fn consolidate(&mut self, with: MemoryDB<DBHasher, DBValue>) {
        self.overlay.consolidate(with);
    }

    fn state(&self, id: &H256) -> Option<Bytes> {
        self.backing
            .get_by_prefix(self.column, &id[0..DB_PREFIX_LEN])
            .map(|b| b.into_vec())
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::InMemoryWithMetrics;
    use hash_db::HashDB;
    use keccak::keccak;
    use JournalDB;

    type TestHashDB = dyn HashDB<DBHasher, DBValue>;

    #[test]
    fn insert_same_in_fork() {
        // history is 1
        let mut jdb = ArchiveDB::new(Arc::new(crate::InMemoryWithMetrics::create(1)), 0);

        let x = TestHashDB::insert(&mut jdb, b"X");
        jdb.commit_batch(1, &keccak(b"1"), None).unwrap();
        jdb.commit_batch(2, &keccak(b"2"), None).unwrap();
        jdb.commit_batch(3, &keccak(b"1002a"), Some((1, keccak(b"1"))))
            .unwrap();
        jdb.commit_batch(4, &keccak(b"1003a"), Some((2, keccak(b"2"))))
            .unwrap();

        TestHashDB::remove(&mut jdb, &x);
        jdb.commit_batch(3, &keccak(b"1002b"), Some((1, keccak(b"1"))))
            .unwrap();
        let x = TestHashDB::insert(&mut jdb, b"X");
        jdb.commit_batch(4, &keccak(b"1003b"), Some((2, keccak(b"2"))))
            .unwrap();

        jdb.commit_batch(5, &keccak(b"1004a"), Some((3, keccak(b"1002a"))))
            .unwrap();
        jdb.commit_batch(6, &keccak(b"1005a"), Some((4, keccak(b"1003a"))))
            .unwrap();

        assert!(TestHashDB::contains(&jdb, &x));
    }

    #[test]
    fn long_history() {
        // history is 3
        let mut jdb = ArchiveDB::new(Arc::new(crate::InMemoryWithMetrics::create(1)), 0);
        let h = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(TestHashDB::contains(&jdb, &h));
        TestHashDB::remove(&mut jdb, &h);
        jdb.commit_batch(1, &keccak(b"1"), None).unwrap();
        assert!(TestHashDB::contains(&jdb, &h));
        jdb.commit_batch(2, &keccak(b"2"), None).unwrap();
        assert!(TestHashDB::contains(&jdb, &h));
        jdb.commit_batch(3, &keccak(b"3"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &h));
        jdb.commit_batch(4, &keccak(b"4"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &h));
    }

    #[test]
    #[should_panic]
    fn multiple_owed_removal_not_allowed() {
        let mut jdb = ArchiveDB::new(Arc::new(crate::InMemoryWithMetrics::create(1)), 0);
        let h = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(TestHashDB::contains(&jdb, &h));
        TestHashDB::remove(&mut jdb, &h);
        TestHashDB::remove(&mut jdb, &h);
        // commit_batch would call journal_under(),
        // and we don't allow multiple owned removals.
        jdb.commit_batch(1, &keccak(b"1"), None).unwrap();
    }

    #[test]
    fn complex() {
        // history is 1
        let mut jdb = ArchiveDB::new(Arc::new(crate::InMemoryWithMetrics::create(1)), 0);

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        let bar = TestHashDB::insert(&mut jdb, b"bar");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(TestHashDB::contains(&jdb, &bar));

        TestHashDB::remove(&mut jdb, &foo);
        TestHashDB::remove(&mut jdb, &bar);
        let baz = TestHashDB::insert(&mut jdb, b"baz");
        jdb.commit_batch(1, &keccak(b"1"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(TestHashDB::contains(&jdb, &bar));
        assert!(TestHashDB::contains(&jdb, &baz));

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        TestHashDB::remove(&mut jdb, &baz);
        jdb.commit_batch(2, &keccak(b"2"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(TestHashDB::contains(&jdb, &baz));

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(3, &keccak(b"3"), Some((2, keccak(b"2"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));

        jdb.commit_batch(4, &keccak(b"4"), Some((3, keccak(b"3"))))
            .unwrap();
    }

    #[test]
    fn fork() {
        // history is 1
        let mut jdb = ArchiveDB::new(Arc::new(crate::InMemoryWithMetrics::create(1)), 0);

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        let bar = TestHashDB::insert(&mut jdb, b"bar");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(TestHashDB::contains(&jdb, &bar));

        TestHashDB::remove(&mut jdb, &foo);
        let baz = TestHashDB::insert(&mut jdb, b"baz");
        jdb.commit_batch(1, &keccak(b"1a"), Some((0, keccak(b"0"))))
            .unwrap();

        TestHashDB::remove(&mut jdb, &bar);
        jdb.commit_batch(1, &keccak(b"1b"), Some((0, keccak(b"0"))))
            .unwrap();

        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(TestHashDB::contains(&jdb, &bar));
        assert!(TestHashDB::contains(&jdb, &baz));

        jdb.commit_batch(2, &keccak(b"2b"), Some((1, keccak(b"1b"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));
    }

    #[test]
    fn overwrite() {
        // history is 1
        let mut jdb = ArchiveDB::new(Arc::new(crate::InMemoryWithMetrics::create(1)), 0);

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(1, &keccak(b"1"), Some((0, keccak(b"0"))))
            .unwrap();
        TestHashDB::insert(&mut jdb, b"foo");
        assert!(TestHashDB::contains(&jdb, &foo));
        jdb.commit_batch(2, &keccak(b"2"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));
        jdb.commit_batch(3, &keccak(b"2"), Some((0, keccak(b"2"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));
    }

    #[test]
    fn fork_same_key() {
        // history is 1
        let mut jdb = ArchiveDB::new(Arc::new(crate::InMemoryWithMetrics::create(1)), 0);
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(1, &keccak(b"1a"), Some((0, keccak(b"0"))))
            .unwrap();

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(1, &keccak(b"1b"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));

        jdb.commit_batch(2, &keccak(b"2a"), Some((1, keccak(b"1a"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));
    }

    #[test]
    fn reopen() {
        let shared_db = Arc::new(crate::InMemoryWithMetrics::create(1));
        let bar = H256::random();

        let foo = {
            let mut jdb = ArchiveDB::new(shared_db.clone(), 0);
            // history is 1
            let foo = TestHashDB::insert(&mut jdb, b"foo");
            jdb.emplace(bar.clone(), (b"bar").to_vec());
            jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
            foo
        };

        {
            let mut jdb = ArchiveDB::new(shared_db.clone(), 0);
            TestHashDB::remove(&mut jdb, &foo);
            jdb.commit_batch(1, &keccak(b"1"), Some((0, keccak(b"0"))))
                .unwrap();
        }

        {
            let mut jdb = ArchiveDB::new(shared_db, 0);
            assert!(TestHashDB::contains(&jdb, &foo));
            assert!(TestHashDB::contains(&jdb, &bar));
            jdb.commit_batch(2, &keccak(b"2"), Some((1, keccak(b"1"))))
                .unwrap();
        }
    }

    #[test]
    fn reopen_remove() {
        let shared_db = Arc::new(crate::InMemoryWithMetrics::create(1));

        let foo = {
            let mut jdb = ArchiveDB::new(shared_db.clone(), 0);
            // history is 1
            let foo = TestHashDB::insert(&mut jdb, b"foo");
            jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
            jdb.commit_batch(1, &keccak(b"1"), Some((0, keccak(b"0"))))
                .unwrap();

            // foo is ancient history.

            TestHashDB::insert(&mut jdb, b"foo");
            jdb.commit_batch(2, &keccak(b"2"), Some((1, keccak(b"1"))))
                .unwrap();
            foo
        };

        {
            let mut jdb = ArchiveDB::new(shared_db, 0);
            TestHashDB::remove(&mut jdb, &foo);
            jdb.commit_batch(3, &keccak(b"3"), Some((2, keccak(b"2"))))
                .unwrap();
            assert!(TestHashDB::contains(&jdb, &foo));
            TestHashDB::remove(&mut jdb, &foo);
            jdb.commit_batch(4, &keccak(b"4"), Some((3, keccak(b"3"))))
                .unwrap();
            jdb.commit_batch(5, &keccak(b"5"), Some((4, keccak(b"4"))))
                .unwrap();
        }
    }

    #[test]
    fn reopen_fork() {
        let shared_db = Arc::new(crate::InMemoryWithMetrics::create(1));
        let (foo, _, _) = {
            let mut jdb = ArchiveDB::new(shared_db.clone(), 0);
            // history is 1
            let foo = TestHashDB::insert(&mut jdb, b"foo");
            let bar = TestHashDB::insert(&mut jdb, b"bar");
            jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
            TestHashDB::remove(&mut jdb, &foo);
            let baz = TestHashDB::insert(&mut jdb, b"baz");
            jdb.commit_batch(1, &keccak(b"1a"), Some((0, keccak(b"0"))))
                .unwrap();

            TestHashDB::remove(&mut jdb, &bar);
            jdb.commit_batch(1, &keccak(b"1b"), Some((0, keccak(b"0"))))
                .unwrap();
            (foo, bar, baz)
        };

        {
            let mut jdb = ArchiveDB::new(shared_db, 0);
            jdb.commit_batch(2, &keccak(b"2b"), Some((1, keccak(b"1b"))))
                .unwrap();
            assert!(TestHashDB::contains(&jdb, &foo));
        }
    }

    #[test]
    fn inject() {
        let mut jdb = ArchiveDB::new(Arc::new(crate::InMemoryWithMetrics::create(1)), 0);
        let key = TestHashDB::insert(&mut jdb, b"dog");
        jdb.inject_batch().unwrap();

        assert_eq!(TestHashDB::get(&jdb, &key).unwrap(), (b"dog").to_vec());
        TestHashDB::remove(&mut jdb, &key);
        jdb.inject_batch().unwrap();

        assert!(TestHashDB::get(&jdb, &key).is_none());
    }

    #[test]
    fn returns_state() {
        let shared_db = Arc::new(InMemoryWithMetrics::create(1));

        let key = {
            let mut jdb = ArchiveDB::new(shared_db.clone(), 0);
            let key = TestHashDB::insert(&mut jdb, b"foo");
            jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
            key
        };

        {
            let jdb = ArchiveDB::new(shared_db, 0);
            let state = jdb.state(&key);
            assert!(state.is_some());
        }
    }
}
