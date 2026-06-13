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

//! `JournalDB` over in-memory overlay

use std::{
    collections::{hash_map::Entry, BTreeMap, HashMap},
    io,
    sync::Arc,
};

use super::{error_negatively_reference_hash, JournalDB, DB_PREFIX_LEN, LATEST_ERA_KEY};
use crate::hasher::DBHasher;
use crate::KeyValueDB;
use bytes::Bytes;
use ethereum_types::H256;
use fastmap::H256FastMap;
use hash_db::HashDB;
use kvdb::{DBTransaction, DBValue};
use memory_db::*;
use parity_util_mem::MallocSizeOf;
use parking_lot::RwLock;
use rlp::{decode, encode, Decodable, DecoderError, Encodable, Rlp, RlpStream};
use util::DatabaseKey;

/// Implementation of the `JournalDB` trait for a disk-backed database with a memory overlay
/// and, possibly, latent-removal semantics.
///
/// Like `OverlayDB`, there is a memory overlay; `commit()` must be called in order to
/// write operations out to disk. Unlike `OverlayDB`, `remove()` operations do not take effect
/// immediately. Rather some age (based on a linear but arbitrary metric) must pass before
/// the removals actually take effect.
///
/// There are two memory overlays:
/// - Transaction overlay contains current transaction data. It is merged with with history
/// overlay on each `commit()`
/// - History overlay contains all data inserted during the history period. When the node
/// in the overlay becomes ancient it is written to disk on `commit()`
///
/// There is also a journal maintained in memory and on the disk as well which lists insertions
/// and removals for each commit during the history period. This is used to track
/// data nodes that go out of history scope and must be written to disk.
///
/// Commit workflow:
/// 1. Create a new journal record from the transaction overlay.
/// 2. Insert each node from the transaction overlay into the History overlay increasing reference
/// count if it is already there. Note that the reference counting is managed by `MemoryDB`
/// 3. Clear the transaction overlay.
/// 4. For a canonical journal record that becomes ancient inserts its insertions into the disk DB
/// 5. For each journal record that goes out of the history scope (becomes ancient) remove its
/// insertions from the history overlay, decreasing the reference counter and removing entry if
/// if reaches zero.
/// 6. For a canonical journal record that becomes ancient delete its removals from the disk only if
/// the removed key is not present in the history overlay.
/// 7. Delete ancient record from memory and disk.

pub struct OverlayRecentDB {
    transaction_overlay: MemoryDB<DBHasher, DBValue>,
    backing: Arc<dyn KeyValueDB>,
    journal_overlay: Arc<RwLock<JournalOverlay>>,
    read_cache: Arc<RwLock<HashMap<H256, Option<DBValue>>>>,
    column: u32,
}

struct DatabaseValue {
    id: H256,
    inserts: Vec<(H256, DBValue)>,
    deletes: Vec<H256>,
}

impl Decodable for DatabaseValue {
    fn decode(rlp: &Rlp) -> Result<Self, DecoderError> {
        let id = rlp.val_at(0)?;
        let inserts = rlp
            .at(1)?
            .iter()
            .map(|r| {
                let k = r.val_at(0)?;
                let v = (r.at(1)?.data()?).to_vec();
                Ok((k, v))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let deletes = rlp.list_at(2)?;

        let value = DatabaseValue {
            id,
            inserts,
            deletes,
        };

        Ok(value)
    }
}

struct DatabaseValueRef<'a> {
    id: &'a H256,
    inserts: &'a [(H256, DBValue)],
    deletes: &'a [H256],
}

impl<'a> Encodable for DatabaseValueRef<'a> {
    fn rlp_append(&self, s: &mut RlpStream) {
        s.begin_list(3);
        s.append(self.id);
        s.begin_list(self.inserts.len());
        for kv in self.inserts {
            s.begin_list(2);
            s.append(&kv.0);
            s.append(&&*kv.1);
        }
        s.append_list(self.deletes);
    }
}

#[derive(PartialEq)]
struct JournalOverlay {
    backing_overlay: MemoryDB<DBHasher, DBValue>, // Nodes added in the history period
    pending_overlay: H256FastMap<DBValue>, // Nodes being transfered from backing_overlay to backing db
    journal: HashMap<u64, Vec<JournalEntry>>,
    latest_era: Option<u64>,
    earliest_era: Option<u64>,
    cumulative_size: usize, // cumulative size of all entries.
}

#[derive(PartialEq, MallocSizeOf)]
struct JournalEntry {
    id: H256,
    insertions: Vec<H256>,
    deletions: Vec<H256>,
}

impl Clone for OverlayRecentDB {
    fn clone(&self) -> OverlayRecentDB {
        OverlayRecentDB {
            transaction_overlay: self.transaction_overlay.clone(),
            backing: self.backing.clone(),
            journal_overlay: self.journal_overlay.clone(),
            read_cache: self.read_cache.clone(),
            column: self.column.clone(),
        }
    }
}

impl OverlayRecentDB {
    /// Create a new instance.
    pub fn new(backing: Arc<dyn KeyValueDB>, col: u32) -> OverlayRecentDB {
        let journal_overlay = Arc::new(RwLock::new(OverlayRecentDB::read_overlay(&*backing, col)));
        OverlayRecentDB {
            transaction_overlay: ::new_memory_db(),
            backing: backing,
            journal_overlay: journal_overlay,
            read_cache: Arc::new(Default::default()),
            column: col,
        }
    }

    #[cfg(test)]
    fn can_reconstruct_refs(&self) -> bool {
        let reconstructed = Self::read_overlay(&*self.backing, self.column);
        let journal_overlay = self.journal_overlay.read();
        journal_overlay.backing_overlay == reconstructed.backing_overlay
            && journal_overlay.pending_overlay == reconstructed.pending_overlay
            && journal_overlay.journal == reconstructed.journal
            && journal_overlay.latest_era == reconstructed.latest_era
            && journal_overlay.cumulative_size == reconstructed.cumulative_size
    }

    fn payload(&self, key: &H256) -> Option<DBValue> {
        self.backing
            .get(self.column, key.as_bytes())
            .expect("Low-level database error. Some issue with your hard disk?")
    }

    fn read_overlay(db: &dyn KeyValueDB, col: u32) -> JournalOverlay {
        let mut journal = HashMap::new();
        let mut overlay = ::new_memory_db();
        let mut count = 0;
        let mut latest_era = None;
        let mut earliest_era = None;
        let mut cumulative_size = 0;
        if let Some(val) = db
            .get(col, &LATEST_ERA_KEY)
            .expect("Low-level database error.")
        {
            let mut era = decode::<u64>(&val).expect("decoding db value failed");
            latest_era = Some(era);
            loop {
                let mut db_key = DatabaseKey { era, index: 0usize };
                while let Some(rlp_data) = db
                    .get(col, &encode(&db_key))
                    .expect("Low-level database error.")
                {
                    trace!("read_overlay: era={}, index={}", era, db_key.index);
                    let value = decode::<DatabaseValue>(&rlp_data).expect(&format!(
                        "read_overlay: Error decoding DatabaseValue era={}, index{}",
                        era, db_key.index
                    ));
                    count += value.inserts.len();
                    let mut inserted_keys = Vec::new();
                    for (k, v) in value.inserts {
                        let short_key = to_short_key(&k);

                        if !overlay.contains(&short_key) {
                            cumulative_size += v.len();
                        }

                        overlay.emplace(short_key, v);
                        inserted_keys.push(k);
                    }
                    journal
                        .entry(era)
                        .or_insert_with(Vec::new)
                        .push(JournalEntry {
                            id: value.id,
                            insertions: inserted_keys,
                            deletions: value.deletes,
                        });
                    db_key.index += 1;
                    earliest_era = Some(era);
                }
                if db_key.index == 0 || era == 0 {
                    break;
                }
                era -= 1;
            }
        }
        trace!(
            "Recovered {} overlay entries, {} journal entries",
            count,
            journal.len()
        );
        JournalOverlay {
            backing_overlay: overlay,
            pending_overlay: HashMap::default(),
            journal: journal,
            latest_era: latest_era,
            earliest_era: earliest_era,
            cumulative_size: cumulative_size,
        }
    }
}

#[inline]
fn to_short_key(key: &H256) -> H256 {
    let mut k = H256::zero();
    k[0..DB_PREFIX_LEN].copy_from_slice(&key[0..DB_PREFIX_LEN]);
    k
}

impl ::traits::KeyedHashDB for OverlayRecentDB {
    fn keys(&self) -> HashMap<H256, i32> {
        let mut ret: HashMap<H256, i32> = self
            .backing
            .iter(self.column)
            .map(|(key, _)| (H256::from_slice(&*key), 1))
            .collect();

        for (key, refs) in self.transaction_overlay.keys() {
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

impl JournalDB for OverlayRecentDB {
    fn boxed_clone(&self) -> Box<dyn JournalDB> {
        Box::new(self.clone())
    }

    fn get_sizes(&self, sizes: &mut BTreeMap<String, usize>) {
        sizes.insert(
            String::from("db_overlay_recent_transactions_size"),
            self.transaction_overlay.len(),
        );

        let overlay = self.journal_overlay.read();
        sizes.insert(
            String::from("db_overlay_recent_backing_size"),
            overlay.backing_overlay.len(),
        );
        sizes.insert(
            String::from("db_overlay_recent_pending_size"),
            overlay.pending_overlay.len(),
        );
        sizes.insert(
            String::from("db_overlay_recent_journal_size"),
            overlay.journal.len(),
        );
    }

    fn journal_size(&self) -> usize {
        self.journal_overlay.read().cumulative_size
    }

    fn is_empty(&self) -> bool {
        self.backing
            .get(self.column, &LATEST_ERA_KEY)
            .expect("Low level database error")
            .is_none()
    }

    fn backing(&self) -> &Arc<dyn KeyValueDB> {
        &self.backing
    }

    fn latest_era(&self) -> Option<u64> {
        self.journal_overlay.read().latest_era
    }

    fn earliest_era(&self) -> Option<u64> {
        self.journal_overlay.read().earliest_era
    }

    // t_nb 9.6
    fn journal_under(&mut self, batch: &mut DBTransaction, now: u64, id: &H256) -> io::Result<u32> {
        trace!(target: "journaldb", "entry: #{} ({})", now, id);

        let mut journal_overlay = self.journal_overlay.write();

        // flush previous changes
        journal_overlay.pending_overlay.clear();

        let mut tx = self.transaction_overlay.drain();
        let inserted_keys: Vec<_> = tx
            .iter()
            .filter_map(|(k, &(_, c))| if c > 0 { Some(k.clone()) } else { None })
            .collect();
        let removed_keys: Vec<_> = tx
            .iter()
            .filter_map(|(k, &(_, c))| if c < 0 { Some(k.clone()) } else { None })
            .collect();
        let ops = inserted_keys.len() + removed_keys.len();

        // Increase counter for each inserted key no matter if the block is canonical or not.
        let insertions: Vec<_> = tx
            .drain()
            .filter_map(|(k, (v, c))| if c > 0 { Some((k, v)) } else { None })
            .collect();

        let encoded_value = {
            let value_ref = DatabaseValueRef {
                id,
                inserts: &insertions,
                deletes: &removed_keys,
            };
            encode(&value_ref)
        };

        for (k, v) in insertions {
            let short_key = to_short_key(&k);
            if !journal_overlay.backing_overlay.contains(&short_key) {
                journal_overlay.cumulative_size += v.len();
            }

            journal_overlay.backing_overlay.emplace(short_key, v);
        }

        let index = journal_overlay.journal.get(&now).map_or(0, |j| j.len());
        let db_key = DatabaseKey { era: now, index };

        batch.put_vec(self.column, &encode(&db_key), encoded_value.to_vec());
        if journal_overlay.latest_era.map_or(true, |e| now > e) {
            trace!(target: "journaldb", "Set latest era to {}", now);
            batch.put_vec(self.column, &LATEST_ERA_KEY, encode(&now).to_vec());
            journal_overlay.latest_era = Some(now);
        }

        if journal_overlay.earliest_era.map_or(true, |e| e > now) {
            trace!(target: "journaldb", "Set earliest era to {}", now);
            journal_overlay.earliest_era = Some(now);
        }

        journal_overlay
            .journal
            .entry(now)
            .or_insert_with(Vec::new)
            .push(JournalEntry {
                id: id.clone(),
                insertions: inserted_keys,
                deletions: removed_keys,
            });
        Ok(ops as u32)
    }

    fn mark_canonical(
        &mut self,
        batch: &mut DBTransaction,
        end_era: u64,
        canon_id: &H256,
    ) -> io::Result<u32> {
        trace!(target: "journaldb", "canonical: #{} ({})", end_era, canon_id);

        let mut journal_overlay = self.journal_overlay.write();
        let journal_overlay = &mut *journal_overlay;

        let mut ops = 0;
        // apply old commits' details
        if let Some(ref mut records) = journal_overlay.journal.get_mut(&end_era) {
            let mut canon_insertions: Vec<(H256, DBValue)> = Vec::new();
            let mut canon_deletions: Vec<H256> = Vec::new();
            let mut overlay_deletions: Vec<H256> = Vec::new();
            let mut index = 0usize;
            for mut journal in records.drain(..) {
                //delete the record from the db
                let db_key = DatabaseKey {
                    era: end_era,
                    index,
                };
                batch.delete(self.column, &encode(&db_key));
                trace!(target: "journaldb", "Delete journal for time #{}.{}: {}, (canon was {}): +{} -{} entries", end_era, index, journal.id, canon_id, journal.insertions.len(), journal.deletions.len());
                {
                    if *canon_id == journal.id {
                        for h in &journal.insertions {
                            if let Some((d, rc)) =
                                journal_overlay.backing_overlay.raw(&to_short_key(h))
                            {
                                if rc > 0 {
                                    canon_insertions.push((h.clone(), d.clone()));
                                    //TODO: optimize this to avoid data copy
                                }
                            }
                        }
                        canon_deletions = journal.deletions;
                    }
                    overlay_deletions.append(&mut journal.insertions);
                }
                index += 1;
            }

            ops += canon_insertions.len();
            ops += canon_deletions.len();

            // apply canon inserts first
            for (k, v) in canon_insertions {
                batch.put(self.column, k.as_bytes(), &v);
                journal_overlay.pending_overlay.insert(to_short_key(&k), v);
            }
            // update the overlay
            for k in overlay_deletions {
                if let Some(val) = journal_overlay
                    .backing_overlay
                    .remove_and_purge(&to_short_key(&k))
                {
                    journal_overlay.cumulative_size -= val.len();
                }
            }
            // apply canon deletions
            for k in canon_deletions {
                if !journal_overlay.backing_overlay.contains(&to_short_key(&k)) {
                    batch.delete(self.column, k.as_bytes());
                }
            }
        }
        journal_overlay.journal.remove(&end_era);
        journal_overlay.backing_overlay.shrink_to_fit();
        // println!("{:?}", self.read_cache.read().len());
        self.read_cache.write().clear();

        if !journal_overlay.journal.is_empty() {
            trace!(target: "journaldb", "Set earliest_era to {}", end_era + 1);
            journal_overlay.earliest_era = Some(end_era + 1);
        }

        Ok(ops as u32)
    }

    fn flush(&self) {
        self.journal_overlay.write().pending_overlay.clear();
    }

    fn inject(&mut self, batch: &mut DBTransaction) -> io::Result<u32> {
        let mut ops = 0;
        for (key, (value, rc)) in self.transaction_overlay.drain() {
            if rc != 0 {
                ops += 1
            }

            match rc {
                0 => {}
                _ if rc > 0 => batch.put(self.column, key.as_bytes(), &value),
                -1 => {
                    if cfg!(debug_assertions)
                        && self.backing.get(self.column, key.as_bytes())?.is_none()
                    {
                        return Err(error_negatively_reference_hash(&key));
                    }
                    batch.delete(self.column, key.as_bytes())
                }
                _ => panic!("Attempted to inject invalid state ({})", rc),
            }
        }

        Ok(ops)
    }

    fn consolidate(&mut self, with: MemoryDB<DBHasher, DBValue>) {
        self.transaction_overlay.consolidate(with);
    }

    fn state(&self, key: &H256) -> Option<Bytes> {
        let journal_overlay = self.journal_overlay.read();
        let key = to_short_key(key);
        journal_overlay
            .backing_overlay
            .get(&key)
            .or_else(|| journal_overlay.pending_overlay.get(&key).map(|d| d.clone()))
            .or_else(|| {
                self.backing
                    .get_by_prefix(self.column, &key[0..DB_PREFIX_LEN])
                    .map(|b| b.into_vec())
            })
    }
}

impl HashDB<DBHasher, DBValue> for OverlayRecentDB {
    fn get(&self, key: &H256) -> Option<DBValue> {
        if let Some((d, rc)) = self.transaction_overlay.raw(key) {
            if rc > 0 {
                return Some(d.clone());
            }
        }
        let v = {
            let journal_overlay = self.journal_overlay.read();
            let key = to_short_key(key);
            journal_overlay
                .backing_overlay
                .get(&key)
                .or_else(|| journal_overlay.pending_overlay.get(&key).cloned())
        };
        if v.is_some() {
            return v;
        }
        if let Some(v) = self.read_cache.read().get(&key) {
            return v.clone();
        }
        let v = self.payload(key);
        self.read_cache.write().insert(key.clone(), v.clone());
        v
    }

    fn contains(&self, key: &H256) -> bool {
        HashDB::<DBHasher, DBValue>::get(self, key).is_some()
    }

    fn insert(&mut self, value: &[u8]) -> H256 {
        self.transaction_overlay.insert(value)
    }
    fn emplace(&mut self, key: H256, value: DBValue) {
        self.transaction_overlay.emplace(key, value);
    }
    fn remove(&mut self, key: &H256) {
        self.transaction_overlay.remove(key);
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use hash_db::HashDB;
    use keccak::keccak;
    use JournalDB;

    type TestHashDB = dyn HashDB<DBHasher, DBValue>;

    fn new_db() -> OverlayRecentDB {
        let backing = Arc::new(crate::InMemoryWithMetrics::create(1));
        OverlayRecentDB::new(backing, 0)
    }

    #[test]
    fn insert_same_in_fork() {
        // history is 1
        let mut jdb = new_db();

        let x = TestHashDB::insert(&mut jdb, b"X");
        jdb.commit_batch(1, &keccak(b"1"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        jdb.commit_batch(2, &keccak(b"2"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        jdb.commit_batch(3, &keccak(b"1002a"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        jdb.commit_batch(4, &keccak(b"1003a"), Some((2, keccak(b"2"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::remove(&mut jdb, &x);
        jdb.commit_batch(3, &keccak(b"1002b"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        let x = TestHashDB::insert(&mut jdb, b"X");
        jdb.commit_batch(4, &keccak(b"1003b"), Some((2, keccak(b"2"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        jdb.commit_batch(5, &keccak(b"1004a"), Some((3, keccak(b"1002a"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        jdb.commit_batch(6, &keccak(b"1005a"), Some((4, keccak(b"1003a"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        assert!(TestHashDB::contains(&jdb, &x));
    }

    #[test]
    fn long_history() {
        // history is 3
        let mut jdb = new_db();
        let h = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &h));
        TestHashDB::remove(&mut jdb, &h);
        jdb.commit_batch(1, &keccak(b"1"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &h));
        jdb.commit_batch(2, &keccak(b"2"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &h));
        jdb.commit_batch(3, &keccak(b"3"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &h));
        jdb.commit_batch(4, &keccak(b"4"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(!TestHashDB::contains(&jdb, &h));
    }

    #[test]
    fn complex() {
        // history is 1
        let mut jdb = new_db();

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        let bar = TestHashDB::insert(&mut jdb, b"bar");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(TestHashDB::contains(&jdb, &bar));

        TestHashDB::remove(&mut jdb, &foo);
        TestHashDB::remove(&mut jdb, &bar);
        let baz = TestHashDB::insert(&mut jdb, b"baz");
        jdb.commit_batch(1, &keccak(b"1"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(TestHashDB::contains(&jdb, &bar));
        assert!(TestHashDB::contains(&jdb, &baz));

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        TestHashDB::remove(&mut jdb, &baz);
        jdb.commit_batch(2, &keccak(b"2"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(!TestHashDB::contains(&jdb, &bar));
        assert!(TestHashDB::contains(&jdb, &baz));

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(3, &keccak(b"3"), Some((2, keccak(b"2"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(!TestHashDB::contains(&jdb, &bar));
        assert!(!TestHashDB::contains(&jdb, &baz));

        jdb.commit_batch(4, &keccak(b"4"), Some((3, keccak(b"3"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
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
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(TestHashDB::contains(&jdb, &bar));

        TestHashDB::remove(&mut jdb, &foo);
        let baz = TestHashDB::insert(&mut jdb, b"baz");
        jdb.commit_batch(1, &keccak(b"1a"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::remove(&mut jdb, &bar);
        jdb.commit_batch(1, &keccak(b"1b"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(TestHashDB::contains(&jdb, &bar));
        assert!(TestHashDB::contains(&jdb, &baz));

        jdb.commit_batch(2, &keccak(b"2b"), Some((1, keccak(b"1b"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(!TestHashDB::contains(&jdb, &baz));
        assert!(!TestHashDB::contains(&jdb, &bar));
    }

    #[test]
    fn overwrite() {
        // history is 1
        let mut jdb = new_db();

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(1, &keccak(b"1"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        TestHashDB::insert(&mut jdb, b"foo");
        assert!(TestHashDB::contains(&jdb, &foo));
        jdb.commit_batch(2, &keccak(b"2"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));
        jdb.commit_batch(3, &keccak(b"2"), Some((0, keccak(b"2"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));
    }

    #[test]
    fn fork_same_key_one() {
        let mut jdb = new_db();
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(1, &keccak(b"1a"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(1, &keccak(b"1b"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(1, &keccak(b"1c"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        assert!(TestHashDB::contains(&jdb, &foo));

        jdb.commit_batch(2, &keccak(b"2a"), Some((1, keccak(b"1a"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));
    }

    #[test]
    fn fork_same_key_other() {
        let mut jdb = new_db();

        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(1, &keccak(b"1a"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(1, &keccak(b"1b"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(1, &keccak(b"1c"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        assert!(TestHashDB::contains(&jdb, &foo));

        jdb.commit_batch(2, &keccak(b"2b"), Some((1, keccak(b"1b"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));
    }

    #[test]
    fn fork_ins_del_ins() {
        let mut jdb = new_db();

        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(1, &keccak(b"1"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(2, &keccak(b"2a"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(2, &keccak(b"2b"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(3, &keccak(b"3a"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(3, &keccak(b"3b"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        jdb.commit_batch(4, &keccak(b"4a"), Some((2, keccak(b"2a"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        jdb.commit_batch(5, &keccak(b"5a"), Some((3, keccak(b"3a"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
    }

    #[test]
    fn reopen() {
        let shared_db = Arc::new(crate::InMemoryWithMetrics::create(1));
        let bar = H256::random();

        let foo = {
            let mut jdb = OverlayRecentDB::new(shared_db.clone(), 0);
            // history is 1
            let foo = TestHashDB::insert(&mut jdb, b"foo");
            jdb.emplace(bar.clone(), (b"bar").to_vec());
            jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
            assert!(jdb.can_reconstruct_refs());
            foo
        };

        {
            let mut jdb = OverlayRecentDB::new(shared_db.clone(), 0);
            TestHashDB::remove(&mut jdb, &foo);
            jdb.commit_batch(1, &keccak(b"1"), Some((0, keccak(b"0"))))
                .unwrap();
            assert!(jdb.can_reconstruct_refs());
        }

        {
            let mut jdb = OverlayRecentDB::new(shared_db.clone(), 0);
            assert!(TestHashDB::contains(&jdb, &foo));
            assert!(TestHashDB::contains(&jdb, &bar));
            jdb.commit_batch(2, &keccak(b"2"), Some((1, keccak(b"1"))))
                .unwrap();
            assert!(jdb.can_reconstruct_refs());
            assert!(!TestHashDB::contains(&jdb, &foo));
        }
    }

    #[test]
    fn insert_delete_insert_delete_insert_expunge() {
        let _ = ::env_logger::try_init();
        let mut jdb = new_db();

        // history is 4
        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(1, &keccak(b"1"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(2, &keccak(b"2"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(3, &keccak(b"3"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(4, &keccak(b"4"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        // expunge foo
        jdb.commit_batch(5, &keccak(b"5"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
    }

    #[test]
    fn forked_insert_delete_insert_delete_insert_expunge() {
        let _ = ::env_logger::try_init();
        let mut jdb = new_db();

        // history is 4
        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(1, &keccak(b"1a"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(1, &keccak(b"1b"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(2, &keccak(b"2a"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(2, &keccak(b"2b"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(3, &keccak(b"3a"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(3, &keccak(b"3b"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(4, &keccak(b"4a"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(4, &keccak(b"4b"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        // expunge foo
        jdb.commit_batch(5, &keccak(b"5"), Some((1, keccak(b"1a"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
    }

    #[test]
    fn broken_assert() {
        let mut jdb = new_db();

        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(1, &keccak(b"1"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        // foo is ancient history.

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(2, &keccak(b"2"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(3, &keccak(b"3"), Some((2, keccak(b"2"))))
            .unwrap(); // BROKEN
        assert!(jdb.can_reconstruct_refs());
        assert!(TestHashDB::contains(&jdb, &foo));

        TestHashDB::remove(&mut jdb, &foo);
        jdb.commit_batch(4, &keccak(b"4"), Some((3, keccak(b"3"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        jdb.commit_batch(5, &keccak(b"5"), Some((4, keccak(b"4"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        assert!(!TestHashDB::contains(&jdb, &foo));
    }

    #[test]
    fn reopen_test() {
        let mut jdb = new_db();
        // history is 4
        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        jdb.commit_batch(1, &keccak(b"1"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        jdb.commit_batch(2, &keccak(b"2"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        jdb.commit_batch(3, &keccak(b"3"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        jdb.commit_batch(4, &keccak(b"4"), Some((0, keccak(b"0"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        // foo is ancient history.

        TestHashDB::insert(&mut jdb, b"foo");
        let bar = TestHashDB::insert(&mut jdb, b"bar");
        jdb.commit_batch(5, &keccak(b"5"), Some((1, keccak(b"1"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        TestHashDB::remove(&mut jdb, &foo);
        TestHashDB::remove(&mut jdb, &bar);
        jdb.commit_batch(6, &keccak(b"6"), Some((2, keccak(b"2"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
        TestHashDB::insert(&mut jdb, b"foo");
        TestHashDB::insert(&mut jdb, b"bar");
        jdb.commit_batch(7, &keccak(b"7"), Some((3, keccak(b"3"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());
    }

    #[test]
    fn reopen_remove_three() {
        let _ = ::env_logger::try_init();

        let shared_db = Arc::new(crate::InMemoryWithMetrics::create(1));
        let foo = keccak(b"foo");

        {
            let mut jdb = OverlayRecentDB::new(shared_db.clone(), 0);
            // history is 1
            TestHashDB::insert(&mut jdb, b"foo");
            jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
            assert!(jdb.can_reconstruct_refs());
            jdb.commit_batch(1, &keccak(b"1"), None).unwrap();
            assert!(jdb.can_reconstruct_refs());

            // foo is ancient history.

            TestHashDB::remove(&mut jdb, &foo);
            jdb.commit_batch(2, &keccak(b"2"), Some((0, keccak(b"0"))))
                .unwrap();
            assert!(jdb.can_reconstruct_refs());
            assert!(TestHashDB::contains(&jdb, &foo));

            TestHashDB::insert(&mut jdb, b"foo");
            jdb.commit_batch(3, &keccak(b"3"), Some((1, keccak(b"1"))))
                .unwrap();
            assert!(jdb.can_reconstruct_refs());
            assert!(TestHashDB::contains(&jdb, &foo));

            // incantation to reopen the db
        };
        {
            let mut jdb = OverlayRecentDB::new(shared_db.clone(), 0);

            TestHashDB::remove(&mut jdb, &foo);
            jdb.commit_batch(4, &keccak(b"4"), Some((2, keccak(b"2"))))
                .unwrap();
            assert!(jdb.can_reconstruct_refs());
            assert!(TestHashDB::contains(&jdb, &foo));

            // incantation to reopen the db
        };
        {
            let mut jdb = OverlayRecentDB::new(shared_db.clone(), 0);

            jdb.commit_batch(5, &keccak(b"5"), Some((3, keccak(b"3"))))
                .unwrap();
            assert!(jdb.can_reconstruct_refs());
            assert!(TestHashDB::contains(&jdb, &foo));

            // incantation to reopen the db
        };
        {
            let mut jdb = OverlayRecentDB::new(shared_db, 0);

            jdb.commit_batch(6, &keccak(b"6"), Some((4, keccak(b"4"))))
                .unwrap();
            assert!(jdb.can_reconstruct_refs());
            assert!(!TestHashDB::contains(&jdb, &foo));
        }
    }

    #[test]
    fn reopen_fork() {
        let shared_db = Arc::new(crate::InMemoryWithMetrics::create(1));

        let (foo, bar, baz) = {
            let mut jdb = OverlayRecentDB::new(shared_db.clone(), 0);
            // history is 1
            let foo = TestHashDB::insert(&mut jdb, b"foo");
            let bar = TestHashDB::insert(&mut jdb, b"bar");
            jdb.commit_batch(0, &keccak(b"0"), None).unwrap();
            assert!(jdb.can_reconstruct_refs());
            TestHashDB::remove(&mut jdb, &foo);
            let baz = TestHashDB::insert(&mut jdb, b"baz");
            jdb.commit_batch(1, &keccak(b"1a"), Some((0, keccak(b"0"))))
                .unwrap();
            assert!(jdb.can_reconstruct_refs());

            TestHashDB::remove(&mut jdb, &bar);
            jdb.commit_batch(1, &keccak(b"1b"), Some((0, keccak(b"0"))))
                .unwrap();
            assert!(jdb.can_reconstruct_refs());
            (foo, bar, baz)
        };

        {
            let mut jdb = OverlayRecentDB::new(shared_db, 0);
            jdb.commit_batch(2, &keccak(b"2b"), Some((1, keccak(b"1b"))))
                .unwrap();
            assert!(jdb.can_reconstruct_refs());
            assert!(TestHashDB::contains(&jdb, &foo));
            assert!(!TestHashDB::contains(&jdb, &baz));
            assert!(!TestHashDB::contains(&jdb, &bar));
        }
    }

    #[test]
    fn insert_older_era() {
        let mut jdb = new_db();
        let foo = TestHashDB::insert(&mut jdb, b"foo");
        jdb.commit_batch(0, &keccak(b"0a"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());

        let bar = TestHashDB::insert(&mut jdb, b"bar");
        jdb.commit_batch(1, &keccak(b"1"), Some((0, keccak(b"0a"))))
            .unwrap();
        assert!(jdb.can_reconstruct_refs());

        TestHashDB::remove(&mut jdb, &bar);
        jdb.commit_batch(0, &keccak(b"0b"), None).unwrap();
        assert!(jdb.can_reconstruct_refs());
        jdb.commit_batch(2, &keccak(b"2"), Some((1, keccak(b"1"))))
            .unwrap();

        assert!(TestHashDB::contains(&jdb, &foo));
        assert!(TestHashDB::contains(&jdb, &bar));
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

    #[test]
    fn earliest_era() {
        let shared_db = Arc::new(crate::InMemoryWithMetrics::create(1));

        // empty DB
        let mut jdb = OverlayRecentDB::new(shared_db.clone(), 0);
        assert!(jdb.earliest_era().is_none());

        // single journalled era.
        let _key = TestHashDB::insert(&mut jdb, b"hello!");
        let mut batch = jdb.backing().transaction();
        jdb.journal_under(&mut batch, 0, &keccak(b"0")).unwrap();
        jdb.backing().write_buffered(batch);

        assert_eq!(jdb.earliest_era(), Some(0));

        // second journalled era.
        let mut batch = jdb.backing().transaction();
        jdb.journal_under(&mut batch, 1, &keccak(b"1")).unwrap();
        jdb.backing().write_buffered(batch);

        assert_eq!(jdb.earliest_era(), Some(0));

        // single journalled era.
        let mut batch = jdb.backing().transaction();
        jdb.mark_canonical(&mut batch, 0, &keccak(b"0")).unwrap();
        jdb.backing().write_buffered(batch);

        assert_eq!(jdb.earliest_era(), Some(1));

        // no journalled eras.
        let mut batch = jdb.backing().transaction();
        jdb.mark_canonical(&mut batch, 1, &keccak(b"1")).unwrap();
        jdb.backing().write_buffered(batch);

        assert_eq!(jdb.earliest_era(), Some(1));

        // reconstructed: no journal entries.
        drop(jdb);
        let jdb = OverlayRecentDB::new(shared_db, 0);
        assert_eq!(jdb.earliest_era(), None);
    }
}
