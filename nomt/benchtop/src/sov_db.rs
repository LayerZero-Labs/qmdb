use crate::backend::Transaction;
use crate::timer::Timer;
use crate::workload::Workload;
use fxhash::{FxHashMap, FxHashSet};
use jmt::KeyHash;
use jmt::{storage::TreeWriter, JellyfishMerkleTree, Version};
use sov_db::state_db::StateDB;
use sov_schema_db::schema::KeyCodec;
use sov_schema_db::snapshot::{DbSnapshot, QueryManager, SnapshotId};
use sov_schema_db::{RawDbReverseIterator, Schema, SchemaKey};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

const SOV_DB_FOLDER: &str = "sov_db";

struct DBQueryManager {
    inner: sov_schema_db::DB,
}

// DbSnapshot uses this to query directly to DB.
// implementing this was necessary because the vanilla SnapshotManager doesn't allow you to
// directly add/commit snapshots.
impl QueryManager for DBQueryManager {
    type Iter<'a, S: Schema>
        = RawDbReverseIterator<'a>
    where
        Self: 'a;
    type RangeIter<'a, S: Schema>
        = RawDbReverseIterator<'a>
    where
        Self: 'a;

    fn get<S: Schema>(
        &self,
        _: SnapshotId,
        key: &impl KeyCodec<S>,
    ) -> anyhow::Result<Option<S::Value>> {
        self.inner.get(key)
    }

    fn iter<S: Schema>(&self, _: SnapshotId) -> anyhow::Result<Self::Iter<'_, S>> {
        self.inner.raw_iter::<S>()
    }
    fn iter_range<S: Schema>(
        &self,
        _: SnapshotId,
        upper_bound: SchemaKey,
    ) -> anyhow::Result<Self::RangeIter<'_, S>> {
        let mut db_iter = self.inner.raw_iter::<S>()?;
        db_iter.seek(upper_bound)?;
        Ok(db_iter)
    }
}

pub struct SovDB {
    trie_qm: Arc<RwLock<DBQueryManager>>,
}

impl SovDB {
    pub fn open(reset: bool) -> Self {
        if reset {
            // Delete previously existing db
            let _ = std::fs::remove_dir_all(SOV_DB_FOLDER);
        }

        // Create the underlying rocks db databases: one for trie nodes, one for key-value flat
        // storage.

        let trie_db_raw = StateDB::<()>::setup_schema_db(SOV_DB_FOLDER).unwrap();

        let trie_qm = DBQueryManager { inner: trie_db_raw };

        SovDB {
            trie_qm: Arc::new(RwLock::new(trie_qm)),
        }
    }

    pub fn execute(&mut self, mut timer: Option<&mut Timer>, workload: &mut dyn Workload) {
        // sov-db's API initializes the StateDB struct afresh for each "block" - it is not meant
        // to be a long-term handle. We do the same here with the following steps.
        // Reads through go through these stages:
        // Native/StateDB -> snapshot -> snapshot manager (parent snapshots - N/A here) -> rocksdb

        let _timer_guard_total = timer.as_mut().map(|t| t.record_span("workload"));

        // 1. Create a "snapshot" (read-write storage overlays) for trie nodes.
        //
        // 0 is snapshot ID. this can be reused safely, as it's never committed to disk.
        let snapshot_id = 0;
        let trie_snapshot =
            DbSnapshot::<DBQueryManager>::new(snapshot_id, self.trie_qm.clone().into());

        // 2. Create higher-level API handles around the snapshot.
        let trie_db = StateDB::with_db_snapshot(trie_snapshot).unwrap();

        // sov-db is an archive DB, where all data from all versions is kept.
        let write_version = trie_db.get_next_version();
        assert!(write_version > 0);
        let read_version = write_version - 1;

        // Actions are applied to jmt and then applied to the backend
        let jmt = JellyfishMerkleTree::<_, sha2::Sha256>::new(&trie_db);

        let mut transaction = Tx {
            timer,
            reads: FxHashSet::default(),
            writes: HashMap::default(),
            jmt,
            version: read_version,
        };
        workload.run_step(&mut transaction);
        let Tx {
            mut timer,
            writes,
            reads,
            jmt,
            ..
        } = transaction;

        let _timer_guard_commit = timer.as_mut().map(|t| t.record_span("commit_and_prove"));

        // 3. various committing/proving actions.

        // prove all reads.
        {
            for key_hash in reads {
                jmt.get_with_proof(key_hash, read_version).unwrap();
            }
        }

        // write preimages to the trie snapshot. must be done first or trie update panics.
        {
            let preimages = writes.iter().map(|(k, v)| (k.clone(), v.preimage()));
            trie_db.put_preimages(preimages).unwrap();
        }

        // apply all trie updates.
        // We are not interested in storing the witness, but we want to measure
        // the time required to create the proof
        {
            let value_set = writes.iter().map(|(k, v)| (k.clone(), v.value()));

            let (_new_root, _proof, tree_update) = jmt
                .put_value_set_with_proof(value_set, write_version)
                .expect("JMT update must succeed");

            trie_db.write_node_batch(&tree_update.node_batch).unwrap();
        }

        // 4. up to now, nothing has been committed to disk. do that by freezing and committing
        //    to underlying DB handle.
        let trie_qm = self.trie_qm.read().unwrap();

        trie_qm
            .inner
            .write_schemas(trie_db.freeze().unwrap().into())
            .unwrap();
    }
}

enum PreparedWrite {
    Delete(Vec<u8>),
    Put(Vec<u8>, Vec<u8>),
}

impl PreparedWrite {
    fn value(&self) -> Option<Vec<u8>> {
        match self {
            PreparedWrite::Delete(_) => None,
            PreparedWrite::Put(_, ref v) => Some(v.clone()),
        }
    }

    // sov-db requires &Vec - don't shoot the messenger.
    fn preimage(&self) -> &Vec<u8> {
        match self {
            PreparedWrite::Delete(ref p) => p,
            PreparedWrite::Put(ref p, _) => p,
        }
    }
}

struct Tx<'a> {
    timer: Option<&'a mut Timer>,
    reads: FxHashSet<KeyHash>,
    writes: FxHashMap<KeyHash, PreparedWrite>,
    jmt: JellyfishMerkleTree<'a, StateDB<DBQueryManager>, sha2::Sha256>,
    version: Version,
}

impl<'a> Transaction for Tx<'a> {
    fn read(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let _timer_guard_read = self.timer.as_mut().map(|t| t.record_span("read"));

        let key = key.to_vec();
        let key_hash = KeyHash::with::<sha2::Sha256>(&key);
        if let Some(value) = self.writes.get(&key_hash).and_then(|v| v.value()) {
            return Some(value);
        }
        self.reads.insert(key_hash);

        // note: this just reads from flat storage and doesn't do a full trie lookup.
        self.jmt.get(key_hash, self.version).unwrap()
    }

    fn note_read(&mut self, key: &[u8], _value: Option<Vec<u8>>) {
        let key_hash = KeyHash::with::<sha2::Sha256>(&key);
        self.reads.insert(key_hash);
    }

    fn write(&mut self, key: &[u8], value: Option<&[u8]>) {
        let key_hash = KeyHash::with::<sha2::Sha256>(&key);
        let write = match value {
            None => PreparedWrite::Delete(key.to_vec()),
            Some(v) => PreparedWrite::Put(key.to_vec(), v.to_vec()),
        };
        self.writes.insert(key_hash, write);
    }
}
