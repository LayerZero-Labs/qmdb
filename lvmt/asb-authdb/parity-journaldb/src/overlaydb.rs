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
    collections::{hash_map::Entry, HashMap},
    io,
    sync::Arc,
};

use super::error_negatively_reference_hash;
use crate::hasher::DBHasher;
use crate::KeyValueDB;
use ethereum_types::H256;
use hash_db::HashDB;
use kvdb::{DBTransaction, DBValue};
use memory_db::*;
use rlp::{decode, encode, Decodable, DecoderError, Encodable, Rlp, RlpStream};

/// Implementation of the `HashDB` trait for a disk-backed database with a memory overlay.
///
/// The operations `insert()` and `remove()` take place on the memory overlay; batches of
/// such operations may be flushed to the disk-backed DB with `commit()` or discarded with
/// `revert()`.
///
/// `lookup()` and `contains()` maintain normal behaviour - all `insert()` and `remove()`
/// queries have an immediate effect in terms of these functions.
#[derive(Clone)]
pub struct OverlayDB {
    overlay: MemoryDB<DBHasher, DBValue>,
    backing: Arc<dyn KeyValueDB>,
    column: u32,
}

struct Payload {
    count: u32,
    value: DBValue,
}

impl Payload {
    fn new(count: u32, value: DBValue) -> Self {
        Payload { count, value }
    }
}

impl Encodable for Payload {
    fn rlp_append(&self, s: &mut RlpStream) {
        s.begin_list(2);
        s.append(&self.count);
        s.append(&&*self.value);
    }
}

impl Decodable for Payload {
    fn decode(rlp: &Rlp) -> Result<Self, DecoderError> {
        let payload = Payload {
            count: rlp.val_at(0)?,
            value: rlp.at(1)?.data()?.to_vec(),
        };

        Ok(payload)
    }
}

impl OverlayDB {
    /// Create a new instance of OverlayDB given a `backing` database.
    pub fn new(backing: Arc<dyn KeyValueDB>, col: u32) -> OverlayDB {
        OverlayDB {
            overlay: ::new_memory_db(),
            backing: backing,
            column: col,
        }
    }

    /// Create a new instance of OverlayDB with an anonymous temporary database.
    #[cfg(test)]
    pub fn new_temp() -> OverlayDB {
        let backing = Arc::new(crate::InMemoryWithMetrics::create(1));
        Self::new(backing, 0)
    }

    /// Commit all operations in a single batch.
    #[cfg(test)]
    pub fn commit(&mut self) -> io::Result<u32> {
        let mut batch = self.backing.transaction();
        let res = self.commit_to_batch(&mut batch)?;
        self.backing.write(batch).map(|_| res).map_err(|e| e.into())
    }

    /// Commit all operations to given batch.
    pub fn commit_to_batch(&mut self, batch: &mut DBTransaction) -> io::Result<u32> {
        let mut ret = 0u32;
        let mut deletes = 0usize;
        for i in self.overlay.drain() {
            let (key, (value, rc)) = i;
            if rc != 0 {
                match self.payload(&key) {
                    Some(x) => {
                        let total_rc: i32 = x.count as i32 + rc;
                        if total_rc < 0 {
                            return Err(error_negatively_reference_hash(&key));
                        }
                        let payload = Payload::new(total_rc as u32, x.value);
                        deletes += if self.put_payload_in_batch(batch, &key, &payload) {
                            1
                        } else {
                            0
                        };
                    }
                    None => {
                        if rc < 0 {
                            return Err(error_negatively_reference_hash(&key));
                        }
                        let payload = Payload::new(rc as u32, value);
                        self.put_payload_in_batch(batch, &key, &payload);
                    }
                };
                ret += 1;
            }
        }
        trace!("OverlayDB::commit() deleted {} nodes", deletes);
        Ok(ret)
    }

    /// Revert all operations on this object (i.e. `insert()`s and `remove()`s) since the
    /// last `commit()`.
    pub fn revert(&mut self) {
        self.overlay.clear();
    }

    /// Get the number of references that would be committed.
    pub fn commit_refs(&self, key: &H256) -> i32 {
        self.overlay.raw(key).map_or(0, |(_, refs)| refs)
    }

    /// Get the refs and value of the given key.
    fn payload(&self, key: &H256) -> Option<Payload> {
        self.backing
            .get(self.column, key.as_bytes())
            .expect("Low-level database error. Some issue with your hard disk?")
            .map(|ref d| decode(d).expect("decoding db value failed"))
    }

    /// Put the refs and value of the given key, possibly deleting it from the db.
    fn put_payload_in_batch(
        &self,
        batch: &mut DBTransaction,
        key: &H256,
        payload: &Payload,
    ) -> bool {
        if payload.count > 0 {
            batch.put(self.column, key.as_bytes(), &encode(payload));
            false
        } else {
            batch.delete(self.column, key.as_bytes());
            true
        }
    }
}

impl crate::KeyedHashDB for OverlayDB {
    fn keys(&self) -> HashMap<H256, i32> {
        let mut ret: HashMap<H256, i32> = self
            .backing
            .iter(self.column)
            .map(|(key, _)| {
                let h = H256::from_slice(&*key);
                let r = self.payload(&h).unwrap().count;
                (h, r as i32)
            })
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

impl HashDB<DBHasher, DBValue> for OverlayDB {
    fn get(&self, key: &H256) -> Option<DBValue> {
        // return ok if positive; if negative, check backing - might be enough references there to make
        // it positive again.
        let k = self.overlay.raw(key);
        let memrc = {
            if let Some((d, rc)) = k {
                if rc > 0 {
                    return Some(d.clone());
                }
                rc
            } else {
                0
            }
        };
        match self.payload(key) {
            Some(x) => {
                if x.count as i32 + memrc > 0 {
                    Some(x.value)
                } else {
                    None
                }
            }
            // Replace above match arm with this once https://github.com/rust-lang/rust/issues/15287 is done.
            //Some((d, rc)) if rc + memrc > 0 => Some(d),
            _ => None,
        }
    }

    fn contains(&self, key: &H256) -> bool {
        // return ok if positive; if negative, check backing - might be enough references there to make
        // it positive again.
        let k = self.overlay.raw(key);
        match k {
            Some((_, rc)) if rc > 0 => true,
            _ => {
                let memrc = k.map_or(0, |(_, rc)| rc);
                match self.payload(key) {
                    Some(x) => x.count as i32 + memrc > 0,
                    // Replace above match arm with this once https://github.com/rust-lang/rust/issues/15287 is done.
                    //Some((d, rc)) if rc + memrc > 0 => true,
                    _ => false,
                }
            }
        }
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

#[cfg(test)]
type TestHashDB = dyn HashDB<DBHasher, DBValue>;

#[test]
fn overlaydb_revert() {
    let mut m = OverlayDB::new_temp();
    let foo = TestHashDB::insert(&mut m, b"foo"); // insert foo.
    let mut batch = m.backing.transaction();
    m.commit_to_batch(&mut batch).unwrap(); // commit - new operations begin here...
    m.backing.write(batch).unwrap();
    let bar = TestHashDB::insert(&mut m, b"bar"); // insert bar.
    TestHashDB::remove(&mut m, &foo); // remove foo.
    assert!(!TestHashDB::contains(&m, &foo)); // foo is gone.
    assert!(TestHashDB::contains(&m, &bar)); // bar is here.
    m.revert(); // revert the last two operations.
    assert!(TestHashDB::contains(&m, &foo)); // foo is here.
    assert!(!TestHashDB::contains(&m, &bar)); // bar is gone.
}

#[test]
fn overlaydb_overlay_insert_and_remove() {
    let mut trie = OverlayDB::new_temp();
    let h = TestHashDB::insert(&mut trie, b"hello world");
    assert_eq!(
        TestHashDB::get(&trie, &h).unwrap(),
        (b"hello world").to_vec()
    );
    TestHashDB::remove(&mut trie, &h);
    assert_eq!(TestHashDB::get(&trie, &h), None);
}

#[test]
fn overlaydb_backing_insert_revert() {
    let mut trie = OverlayDB::new_temp();
    let h = TestHashDB::insert(&mut trie, b"hello world");
    assert_eq!(
        TestHashDB::get(&trie, &h).unwrap(),
        (b"hello world").to_vec()
    );
    trie.commit().unwrap();
    assert_eq!(
        TestHashDB::get(&trie, &h).unwrap(),
        (b"hello world").to_vec()
    );
    trie.revert();
    assert_eq!(
        TestHashDB::get(&trie, &h).unwrap(),
        (b"hello world").to_vec()
    );
}

#[test]
fn overlaydb_backing_remove() {
    let mut trie = OverlayDB::new_temp();
    let h = TestHashDB::insert(&mut trie, b"hello world");
    trie.commit().unwrap();
    TestHashDB::remove(&mut trie, &h);
    assert_eq!(TestHashDB::get(&trie, &h), None);
    trie.commit().unwrap();
    assert_eq!(TestHashDB::get(&trie, &h), None);
    trie.revert();
    assert_eq!(TestHashDB::get(&trie, &h), None);
}

#[test]
fn overlaydb_backing_remove_revert() {
    let mut trie = OverlayDB::new_temp();
    let h = TestHashDB::insert(&mut trie, b"hello world");
    trie.commit().unwrap();
    TestHashDB::remove(&mut trie, &h);
    assert_eq!(TestHashDB::get(&trie, &h), None);
    trie.revert();
    assert_eq!(
        TestHashDB::get(&trie, &h).unwrap(),
        (b"hello world").to_vec()
    );
}

#[test]
fn overlaydb_negative() {
    let mut trie = OverlayDB::new_temp();
    let h = TestHashDB::insert(&mut trie, b"hello world");
    trie.commit().unwrap();
    TestHashDB::remove(&mut trie, &h);
    TestHashDB::remove(&mut trie, &h); //bad - sends us into negative refs.
    assert_eq!(TestHashDB::get(&trie, &h), None);
    assert!(trie.commit().is_err());
}

#[test]
fn overlaydb_complex() {
    let mut trie = OverlayDB::new_temp();
    let hfoo = TestHashDB::insert(&mut trie, b"foo");
    assert_eq!(TestHashDB::get(&trie, &hfoo).unwrap(), (b"foo").to_vec());
    let hbar = TestHashDB::insert(&mut trie, b"bar");
    assert_eq!(TestHashDB::get(&trie, &hbar).unwrap(), (b"bar").to_vec());
    trie.commit().unwrap();
    assert_eq!(TestHashDB::get(&trie, &hfoo).unwrap(), (b"foo").to_vec());
    assert_eq!(TestHashDB::get(&trie, &hbar).unwrap(), (b"bar").to_vec());
    TestHashDB::insert(&mut trie, b"foo"); // two refs
    assert_eq!(TestHashDB::get(&trie, &hfoo).unwrap(), (b"foo").to_vec());
    trie.commit().unwrap();
    assert_eq!(TestHashDB::get(&trie, &hfoo).unwrap(), (b"foo").to_vec());
    assert_eq!(TestHashDB::get(&trie, &hbar).unwrap(), (b"bar").to_vec());
    TestHashDB::remove(&mut trie, &hbar); // zero refs - delete
    assert_eq!(TestHashDB::get(&trie, &hbar), None);
    TestHashDB::remove(&mut trie, &hfoo); // one ref - keep
    assert_eq!(TestHashDB::get(&trie, &hfoo).unwrap(), (b"foo").to_vec());
    trie.commit().unwrap();
    assert_eq!(TestHashDB::get(&trie, &hfoo).unwrap(), (b"foo").to_vec());
    TestHashDB::remove(&mut trie, &hfoo); // zero ref - would delete, but...
    assert_eq!(TestHashDB::get(&trie, &hfoo), None);
    TestHashDB::insert(&mut trie, b"foo"); // one ref - keep after all.
    assert_eq!(TestHashDB::get(&trie, &hfoo).unwrap(), (b"foo").to_vec());
    trie.commit().unwrap();
    assert_eq!(TestHashDB::get(&trie, &hfoo).unwrap(), (b"foo").to_vec());
    TestHashDB::remove(&mut trie, &hfoo); // zero ref - delete
    assert_eq!(TestHashDB::get(&trie, &hfoo), None);
    trie.commit().unwrap(); //
    assert_eq!(TestHashDB::get(&trie, &hfoo), None);
}
