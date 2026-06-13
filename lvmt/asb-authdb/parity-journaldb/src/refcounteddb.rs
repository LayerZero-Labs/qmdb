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

//! Disk-backed, ref-counted `JournalDB` implementation.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::Arc,
};

use super::{traits::JournalDB, LATEST_ERA_KEY};
use crate::hasher::DBHasher;
use crate::KeyValueDB;
use bytes::Bytes;
use ethereum_types::H256;
use hash_db::HashDB;
use kvdb::{DBTransaction, DBValue};
use memory_db::MemoryDB;
use overlaydb::OverlayDB;
use parity_util_mem::{allocators::new_malloc_size_ops, MallocSizeOf};
use rlp::{decode, encode};
use util::{DatabaseKey, DatabaseValueRef, DatabaseValueView};
use DB_PREFIX_LEN;

/// Implementation of the `HashDB` trait for a disk-backed database with a memory overlay
/// and latent-removal semantics.
///
/// Like `OverlayDB`, there is a memory overlay; `commit()` must be called in order to
/// write operations out to disk. Unlike `OverlayDB`, `remove()` operations do not take effect
/// immediately. Rather some age (based on a linear but arbitrary metric) must pass before
/// the removals actually take effect.
///
/// journal format:
/// ```text
/// [era, 0] => [ id, [insert_0, ...], [remove_0, ...] ]
/// [era, 1] => [ id, [insert_0, ...], [remove_0, ...] ]
/// [era, n] => [ ... ]
/// ```
///
/// when we make a new commit, we journal the inserts and removes.
/// for each `end_era` that we journaled that we are no passing by,
/// we remove all of its removes assuming it is canonical and all
/// of its inserts otherwise.
// TODO: store last_era, reclaim_period.
pub struct RefCountedDB {
    forward: OverlayDB,
    backing: Arc<dyn KeyValueDB>,
    latest_era: Option<u64>,
    inserts: Vec<H256>,
    removes: Vec<H256>,
    column: u32,
}

impl RefCountedDB {
    /// Create a new instance given a `backing` database.
    pub fn new(backing: Arc<dyn KeyValueDB>, column: u32) -> RefCountedDB {
        let latest_era = backing
            .get(column, &LATEST_ERA_KEY)
            .expect("Low-level database error.")
            .map(|v| decode::<u64>(&v).expect("decoding db value failed"));

        RefCountedDB {
            forward: OverlayDB::new(backing.clone(), column),
            backing,
            inserts: vec![],
            removes: vec![],
            latest_era,
            column,
        }
    }
}

impl HashDB<DBHasher, DBValue> for RefCountedDB {
    fn get(&self, key: &H256) -> Option<DBValue> {
        HashDB::<DBHasher, DBValue>::get(&self.forward, key)
    }
    fn contains(&self, key: &H256) -> bool {
        HashDB::<DBHasher, DBValue>::contains(&self.forward, key)
    }
    fn insert(&mut self, value: &[u8]) -> H256 {
        let r = HashDB::<DBHasher, DBValue>::insert(&mut self.forward, value);
        self.inserts.push(r.clone());
        r
    }
    fn emplace(&mut self, key: H256, value: DBValue) {
        self.inserts.push(key.clone());
        HashDB::<DBHasher, DBValue>::emplace(&mut self.forward, key, value);
    }

    fn remove(&mut self, key: &H256) {
        self.removes.push(key.clone());
    }
}

impl ::traits::KeyedHashDB for RefCountedDB {
    fn keys(&self) -> HashMap<H256, i32> {
        self.forward.keys()
    }
}

impl JournalDB for RefCountedDB {
    fn boxed_clone(&self) -> Box<dyn JournalDB> {
        Box::new(RefCountedDB {
            forward: self.forward.clone(),
            backing: self.backing.clone(),
            latest_era: self.latest_era,
            inserts: self.inserts.clone(),
            removes: self.removes.clone(),
            column: self.column.clone(),
        })
    }

    fn get_sizes(&self, sizes: &mut BTreeMap<String, usize>) {
        let mut ops = new_malloc_size_ops();
        sizes.insert(
            String::from("db_ref_counted_inserts"),
            self.inserts.size_of(&mut ops),
        );
        sizes.insert(
            String::from("db_ref_counted_removes"),
            self.removes.size_of(&mut ops),
        );
    }

    fn is_empty(&self) -> bool {
        self.latest_era.is_none()
    }

    fn backing(&self) -> &Arc<dyn KeyValueDB> {
        &self.backing
    }

    fn latest_era(&self) -> Option<u64> {
        self.latest_era
    }

    fn journal_under(&mut self, batch: &mut DBTransaction, now: u64, id: &H256) -> io::Result<u32> {
        // record new commit's details.
        let mut db_key = DatabaseKey {
            era: now,
            index: 0usize,
        };
        let mut last;

        while self
            .backing
            .get(self.column, {
                last = encode(&db_key);
                &last
            })?
            .is_some()
        {
            db_key.index += 1;
        }

        {
            let value_ref = DatabaseValueRef {
                id,
                inserts: &self.inserts,
                deletes: &self.removes,
            };

            batch.put(self.column, &last, &encode(&value_ref));
        }

        let ops = self.inserts.len() + self.removes.len();

        trace!(target: "rcdb", "new journal for time #{}.{} => {}: inserts={:?}, removes={:?}", now, db_key.index, id, self.inserts, self.removes);

        self.inserts.clear();
        self.removes.clear();

        if self.latest_era.map_or(true, |e| now > e) {
            batch.put(self.column, &LATEST_ERA_KEY, &encode(&now));
            self.latest_era = Some(now);
        }

        Ok(ops as u32)
    }

    fn mark_canonical(
        &mut self,
        batch: &mut DBTransaction,
        end_era: u64,
        canon_id: &H256,
    ) -> io::Result<u32> {
        // apply old commits' details
        let mut db_key = DatabaseKey {
            era: end_era,
            index: 0usize,
        };
        let mut last;
        while let Some(rlp_data) = {
            self.backing.get(self.column, {
                last = encode(&db_key);
                &last
            })?
        } {
            let view = DatabaseValueView::from_rlp(&rlp_data);
            let our_id = view.id().expect("rlp read from db; qed");
            let to_remove = if canon_id == &our_id {
                view.deletes()
            } else {
                view.inserts()
            }
            .expect("rlp read from db; qed");
            trace!(target: "rcdb", "delete journal for time #{}.{}=>{}, (canon was {}): deleting {:?}", end_era, db_key.index, our_id, canon_id, to_remove);
            for i in &to_remove {
                HashDB::<DBHasher, DBValue>::remove(&mut self.forward, i);
            }
            batch.delete(self.column, &last);
            db_key.index += 1;
        }

        let r = self.forward.commit_to_batch(batch)?;
        Ok(r)
    }

    fn inject(&mut self, batch: &mut DBTransaction) -> io::Result<u32> {
        self.inserts.clear();
        for remove in self.removes.drain(..) {
            HashDB::<DBHasher, DBValue>::remove(&mut self.forward, &remove);
        }
        self.forward.commit_to_batch(batch)
    }

    fn consolidate(&mut self, mut with: MemoryDB<DBHasher, DBValue>) {
        for (key, (value, rc)) in with.drain() {
            for _ in 0..rc {
                self.emplace(key, value.clone());
            }

            for _ in rc..0 {
                HashDB::<DBHasher, DBValue>::remove(self, &key);
            }
        }
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
    use hash_db::HashDB;
    use keccak::keccak;
    use JournalDB;

    type TestHashDB = dyn HashDB<DBHasher, DBValue>;

    fn new_db() -> RefCountedDB {
        let backing = Arc::new(crate::InMemoryWithMetrics::create(1));
        RefCountedDB::new(backing, 0)
    }

    #[test]
    fn long_history() {
        // history is 3
        let mut jdb = new_db();
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
        assert!(!TestHashDB::contains(&jdb, &h));
    }

    #[test]
    fn latest_era_should_work() {
        // history is 3
        let mut jdb = new_db();
        assert_eq!(jdb.latest_era(), None);
        let h = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert_eq!(jdb.latest_era(), Some(0));
        TestHashDB::remove(&mut jdb, &h);
        jdb.commit_batch(1, &keccak(b"1"), None).unwrap();
        assert_eq!(jdb.latest_era(), Some(1));
        jdb.commit_batch(2, &keccak(b"2"), None).unwrap();
        assert_eq!(jdb.latest_era(), Some(2));
        jdb.commit_batch(3, &keccak(b"3"), Some((0, keccak(b"0"))))
            .unwrap();
        assert_eq!(jdb.latest_era(), Some(3));
        jdb.commit_batch(4, &keccak(b"4"), Some((1, keccak(b"1"))))
            .unwrap();
        assert_eq!(jdb.latest_era(), Some(4));
    }

    #[test]
    fn complex() {
        // history is 1
        let mut jdb = new_db();

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
        assert!(!TestHashDB::contains(&jdb, &bar));
        assert!(TestHashDB::contains(&jdb, &baz));

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(3, &keccak(b"3"), Some((2, keccak(b"2"))))
            .unwrap();
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(!TestHashDB::contains(&jdb, &bar));
        assert!(!TestHashDB::contains(&jdb, &baz));

        jdb.commit_batch(4, &keccak(b"4"), Some((3, keccak(b"3"))))
            .unwrap();
        assert!(!TestHashDB::contains(&jdb, &foo));
        assert!(!TestHashDB::contains(&jdb, &bar));
        assert!(!TestHashDB::contains(&jdb, &baz));
    }

    #[test]
    fn fork() {
        // history is 1
        let mut jdb = new_db();

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
        assert!(!TestHashDB::contains(&jdb, &baz));
        assert!(!TestHashDB::contains(&jdb, &bar));
    }

    #[test]
    fn inject() {
        let mut jdb = new_db();
        let key = TestHashDB::insert(&mut jdb, b"dog");
        jdb.inject_batch().unwrap();

        assert_eq!(TestHashDB::get(&jdb, &key).unwrap(), (b"dog").to_vec());
        TestHashDB::remove(&mut jdb, &key);
        jdb.inject_batch().unwrap();

        assert!(TestHashDB::get(&jdb, &key).is_none());
    }
}
