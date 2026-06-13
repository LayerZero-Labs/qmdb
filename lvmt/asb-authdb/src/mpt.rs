use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use hash_db::Hasher;
use kvdb::{DBKey, DBOp, DBTransaction, KeyValueDB};
use patricia_trie_ethereum::RlpNodeCodec;
use primitive_types::H256;
use trie_db::{NodeCodec, Trie, TrieMut};

use parity_journaldb::{Algorithm, DBHasher, JournalDB};
use parity_scale_codec::KeyedVec;

use asb_options::Options;
use asb_profile::CounterTrait;
use authdb::AuthDB;

pub type TrieDBMut<'db> = trie_db::TrieDBMut<'db, DBHasher, RlpNodeCodec<DBHasher>>;
pub type TrieDB<'db> = trie_db::TrieDB<'db, DBHasher, RlpNodeCodec<DBHasher>>;

pub struct MptDB {
    backing: Arc<dyn KeyValueDB>,
    db: Arc<RefCell<Box<dyn JournalDB>>>,
    root: H256,
    epoch: usize,
    print_root_period: Option<usize>,
    journal_epoch: usize,
}

fn epoch_hash(epoch: usize) -> H256 {
    DBHasher::hash(&epoch.to_le_bytes())
}

pub(crate) fn new(backend: Arc<dyn KeyValueDB>, opts: &Options) -> MptDB {
    let db = parity_journaldb::new(backend.clone(), Algorithm::OverlayRecent, 0);
    let db = Arc::new(RefCell::new(db));
    let print_root_period = if opts.print_root {
        Some(opts.report_epoch)
    } else {
        None
    };
    let root = if let Some(value) = backend.get([0u8; 256].to_vec()) {
        H256::from_slice(&value)
    } else {
        RlpNodeCodec::<DBHasher>::hashed_null_node()
    };

    let journal_epoch = 0;

    MptDB {
        db,
        backing: backend,
        root,
        epoch: 0,
        print_root_period,
        journal_epoch,
    }
}

impl AuthDB for MptDB {
    // This logic is in function `require_or_from` of OpenEthereum
    fn get(&self, key: Vec<u8>) -> Option<Box<[u8]>> {
        let db = self.db.borrow();
        let hash_db = &db.as_hash_db();

        let trie = TrieDB::new(hash_db, &self.root).unwrap();
        trie.get(key.as_slice())
            .unwrap()
            .map(|x| x.into_vec().into_boxed_slice())
    }

    // This logic is in function `commit` in `ethcore/src/state/run` of OpenEthereum
    fn set(&mut self, key: Vec<u8>, value: Vec<u8>) {
        let mut db = self.db.borrow_mut();
        let hash_db = db.as_hash_db_mut();

        let mut trie = TrieDBMut::from_existing(hash_db, &mut self.root).unwrap();
        trie.insert(key.as_slice(), value.as_slice()).unwrap();
    }

    // This logic is in function `commit` in `ethcore/src/state/run` of OpenEthereum
    fn commit(&mut self, index: usize) {
        self.epoch = index;

        let mut batch = DBTransaction::new();
        let mut db = self.db.borrow_mut();

        // The third parameter is not used in archive journal db. We feed an arbitrary data.
        db.journal_under(&mut batch, index as u64, &epoch_hash(index))
            .unwrap();
        if let Some(old_index) = index.checked_sub(self.journal_epoch) {
            db.mark_canonical(&mut batch, old_index as u64, &epoch_hash(old_index))
                .unwrap();
        }
        db.backing().write(batch).unwrap();
        db.flush();

        if let Some(period) = self.print_root_period {
            if index % period == 0 {
                println!("Root {:?}", self.root);
            }
        }
    }

    fn flush_all(&mut self) {
        let mut batch = DBTransaction::new();
        let mut db = self.db.borrow_mut();
        for i in (0..self.journal_epoch).into_iter().rev() {
            let index = self.epoch - i;
            db.mark_canonical(&mut batch, index as u64, &epoch_hash(index))
                .unwrap();
        }
        batch.ops.push(DBOp::Insert {
            col: 0,
            key: DBKey::from_slice(&[0u8; 256]),
            value: self.root.to_keyed_vec(&[]),
        });
        db.backing().write(batch).unwrap();
        db.flush();
    }

    fn backend(&self) -> Option<&dyn KeyValueDB> {
        Some(&*self.backing)
    }
}

pub struct MptCounter {
    journal_db: Arc<RefCell<Box<dyn JournalDB>>>,
}

impl MptCounter {
    pub fn from_mpt_db(mpt_db: &MptDB) -> Self {
        Self {
            journal_db: mpt_db.db.clone(),
        }
    }
}

impl CounterTrait for MptCounter {
    fn report(&mut self) -> String {
        let mut sizes = BTreeMap::new();
        self.journal_db.borrow().get_sizes(&mut sizes);
        format!(
            "Recent backing size: {}",
            sizes.get("db_overlay_recent_backing_size").unwrap()
        )
    }
}
