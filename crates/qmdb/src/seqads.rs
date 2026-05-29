use crate::config::Config;
use crate::metadb::MetaInfo;
use crate::tasks::helpers::SimpleTask;
use crate::tasks::tasksmanager::TasksManager;
use crate::utils::writebatch::WriteBatch;
use crate::{AdsWrap, SharedAdsWrap, ADS};
use dashmap::DashMap;
use parking_lot::RwLock;
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_WRITE};
use qmdb_common::entryfile::entry::{entry_key_hash, EntryBz};
use qmdb_common::entryfile::readbuf::ReadBuf;
use qmdb_common::task::taskid::join_task_id;
use qmdb_common::utils::changeset::ChangeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Sequential API for QMDB.
///
/// Provides a simple sequential interface on top of QMDB. Unlike the pipelined
/// `AdsWrap`, `SeqAds` waits for each block to finish before moving to the next
/// one and offers built-in read-your-writes semantics.
///
/// # Important Notes
///
/// - Each `WriteBatch` stores a single operation per key (later inserts overwrite
///   earlier ones inside the batch).
/// - Deleting a non-existent key will return an error.
/// - Thread safety: `write()` requires `&mut self` (exclusive access), while
///   `read()` only needs `&self` (shared access). Rust's borrow checker prevents
///   data races.
pub struct SeqAds {
    ads: AdsWrap<SimpleTask>,

    shared_ads: SharedAdsWrap,

    cache: HashMap<Box<[u8]>, Option<Box<[u8]>>>,

    current_height: Option<i64>,

    tx_count: usize,

    current_task_index: usize,

    key_to_existence: DashMap<Box<[u8]>, bool>,

    tasks_manager: Option<Arc<TasksManager<SimpleTask>>>,
}

impl SeqAds {
    /// Creates a new sequential wrapper around `AdsWrap`.
    pub fn new(config: &Config) -> Self {
        let ads = AdsWrap::new(config);
        let shared_ads = ads.get_shared();
        Self {
            ads,
            shared_ads,
            cache: HashMap::new(),
            current_height: None,
            tx_count: 0,
            current_task_index: 0,
            key_to_existence: DashMap::new(),
            tasks_manager: None,
        }
    }

    pub fn start_block(&mut self, height: i64, tx_count: usize) -> Result<(), String> {
        if self.current_height.is_some() || self.tasks_manager.is_some() {
            return Err("previous block not ended".to_string());
        }
        let slot_count = tx_count.max(1);
        let tasks_manager = Arc::new(TasksManager::with_size(slot_count));
        let (started, pending_meta) = self.ads.start_block(height, tasks_manager.clone());
        if !started {
            return Err(format!("failed to start block at height {height}"));
        }
        if let Some(meta) = pending_meta {
            return Err(format!(
                "sequential mode violation: block {} still finalizing (extra: {})",
                meta.curr_height, meta.extra_data
            ));
        }

        self.shared_ads = self.ads.get_shared();
        self.shared_ads.insert_extra_data(height, "".to_owned());
        self.current_height = Some(height);
        self.tx_count = tx_count;
        self.current_task_index = 0;
        self.cache.clear();
        self.key_to_existence.clear();
        self.tasks_manager = Some(tasks_manager);

        Ok(())
    }

    pub fn read(&self, key: &[u8], out: &mut ReadBuf) -> bool {
        out.clear();

        if self.current_height.is_none() {
            return false;
        }

        if let Some(entry) = self.cache.get(key) {
            match entry {
                Some(value) => {
                    out.extend_from_slice(value);
                    return true;
                }
                None => return false,
            }
        }

        let found = self.read_value_from_previous_block(key, out);
        self.key_to_existence.insert(Self::boxed_key(key), found);
        found
    }

    /// Writes a batch of operations (corresponding to one transaction).
    ///
    /// # Important
    ///
    /// - Each key should appear at most once in the batch (later inserts overwrite earlier ones)
    /// - Deleting a non-existent key will return an error
    /// - Changes are immediately visible to subsequent `read()` calls
    pub fn write(&mut self, batch: WriteBatch) -> Result<(), String> {
        let height = self
            .current_height
            .ok_or_else(|| "no active block, call start_block() first".to_string())?;

        if self.current_task_index >= self.tx_count {
            return Err("all transactions already submitted".to_string());
        }

        if batch.operations.is_empty() {
            return Err("WriteBatch is empty".to_string());
        }

        let change_set = self.batch_to_changeset(&batch)?;
        let tasks_manager = self
            .tasks_manager
            .as_ref()
            .ok_or_else(|| "tasks manager not initialized".to_string())?;

        tasks_manager.set_task(self.current_task_index, SimpleTask::new(vec![change_set]));

        let is_last = self.current_task_index + 1 == self.tx_count;
        let task_id = join_task_id(height, self.current_task_index, is_last);
        self.shared_ads.add_task(task_id);

        self.current_task_index += 1;
        Ok(())
    }

    pub fn end_block(&mut self) -> Result<Arc<MetaInfo>, String> {
        let height = self
            .current_height
            .ok_or_else(|| "no active block to end".to_string())?;
        if self.tasks_manager.is_none() {
            return Err("task manager missing; did you call start_block?".to_string());
        }

        if self.current_task_index != self.tx_count {
            return Err(format!(
                "transaction count mismatch: expected {}, submitted {}",
                self.tx_count, self.current_task_index
            ));
        }

        if self.tx_count == 0 {
            if let Some(manager) = self.tasks_manager.as_ref() {
                manager.set_task(0, SimpleTask::new(vec![]));
            }
            let task_id = join_task_id(height, 0, true);
            self.shared_ads.add_task(task_id);
        }

        let meta_info = loop {
            let finished = self.ads.flush();
            if let Some(meta) = finished.into_iter().find(|meta| meta.curr_height == height) {
                break meta;
            }
            thread::sleep(Duration::from_millis(5));
        };

        self.tasks_manager = None;
        self.reset_block_state();
        Ok(meta_info)
    }

    pub fn get_metadb(&self) -> Arc<RwLock<crate::metadb::MetaDB>> {
        self.ads.get_metadb()
    }

    pub fn get_shared(&self) -> SharedAdsWrap {
        self.shared_ads.clone()
    }

    // ===== Helper methods =====

    fn batch_to_changeset(&mut self, batch: &WriteBatch) -> Result<ChangeSet, String> {
        if batch.operations.is_empty() {
            return Err("WriteBatch is empty".to_string());
        }

        let mut change_set = ChangeSet::new();
        for (key, value_opt) in batch.operations.iter() {
            let key_slice = key.as_ref();
            let existed = if let Some(entry) = self.cache.get(key_slice) {
                entry.is_some()
            } else {
                self.ensure_key_existence_known(key_slice);
                self.key_to_existence
                    .get(key_slice)
                    .map(|entry| *entry.value())
                    .unwrap_or(false)
            };
            let key_hash = entry_key_hash(key_slice);

            match value_opt {
                Some(value) => {
                    let op = if existed { OP_WRITE } else { OP_CREATE };
                    change_set.add_op(op, &key_hash, key_slice, value.as_ref(), None);
                    self.update_cache_entry(key_slice, Some(value.as_ref()));
                }
                None => {
                    if !existed {
                        return Err("cannot delete non-existent key".to_string());
                    }
                    change_set.add_op(OP_DELETE, &key_hash, key_slice, &[], None);
                    self.update_cache_entry(key_slice, None);
                }
            }
        }
        change_set.sort();
        Ok(change_set)
    }

    fn check_key_existence(&self, key: &[u8]) -> Option<bool> {
        self.key_to_existence.get(key).map(|entry| *entry.value())
    }

    fn ensure_key_existence_known(&mut self, key: &[u8]) {
        if self.key_to_existence.get(key).is_some() {
            return;
        }
        if let Some(entry) = self.cache.get(key) {
            self.key_to_existence
                .insert(Self::boxed_key(key), entry.is_some());
            return;
        }

        let mut buf = ReadBuf::new();
        let exists = self.read_value_from_previous_block(key, &mut buf);
        self.key_to_existence.insert(Self::boxed_key(key), exists);
    }

    fn read_value_from_previous_block(&self, key: &[u8], out: &mut ReadBuf) -> bool {
        out.clear();
        let Some(height) = self.current_height else {
            return false;
        };
        if height <= 0 {
            return false;
        }

        let read_height = height - 1;
        let mut read_buf = ReadBuf::new();
        let key_hash = entry_key_hash(key);
        self.shared_ads
            .read_entry(read_height, &key_hash, key, &mut read_buf);
        if read_buf.is_empty() {
            return false;
        }

        let entry = EntryBz {
            bz: read_buf.as_slice(),
        };
        let value = entry.value();
        out.extend_from_slice(value);
        true
    }

    fn update_cache_entry(&mut self, key: &[u8], value: Option<&[u8]>) {
        let exists = value.is_some();
        let cache_value = value.map(|bytes| bytes.to_vec().into_boxed_slice());
        self.cache.insert(Self::boxed_key(key), cache_value);
        self.key_to_existence.insert(Self::boxed_key(key), exists);
    }

    fn reset_block_state(&mut self) {
        self.current_height = None;
        self.tx_count = 0;
        self.current_task_index = 0;
        self.cache.clear();
        self.key_to_existence.clear();
        self.tasks_manager = None;
        self.shared_ads = self.ads.get_shared();
    }

    fn boxed_key(key: &[u8]) -> Box<[u8]> {
        key.to_vec().into_boxed_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AdsCore;
    use serial_test::serial;
    use tempfile::Builder;

    fn create_seqads_instance() -> (tempfile::TempDir, SeqAds) {
        let temp_dir = Builder::new()
            .prefix("seqads-test")
            .tempdir()
            .expect("temp dir");
        let mut config = Config::from_dir(temp_dir.path().to_str().unwrap());
        config.with_twig_file = true;
        AdsCore::init_dir(&config);
        let seqads = SeqAds::new(&config);
        (temp_dir, seqads)
    }

    fn put_bytes(batch: &mut WriteBatch, key: &[u8], value: &[u8]) {
        batch.put(key.to_vec(), value.to_vec());
    }

    fn delete_key(batch: &mut WriteBatch, key: &[u8]) {
        batch.delete(key.to_vec());
    }

    #[test]
    #[serial]
    fn test_seqads_basic_workflow() {
        let (_temp_dir, mut seqads) = create_seqads_instance();
        let mut buf = ReadBuf::new();

        seqads.start_block(1, 2).unwrap();

        let mut batch1 = WriteBatch::new();
        put_bytes(&mut batch1, b"alpha", b"value1");
        seqads.write(batch1).unwrap();
        assert!(seqads.read(b"alpha", &mut buf));
        assert_eq!(buf.as_slice(), b"value1");

        let mut batch2 = WriteBatch::new();
        put_bytes(&mut batch2, b"beta", b"value2");
        seqads.write(batch2).unwrap();

        let meta = seqads.end_block().unwrap();
        assert_eq!(meta.curr_height, 1);

        seqads.start_block(2, 1).unwrap();
        assert!(seqads.read(b"alpha", &mut buf));
        assert_eq!(buf.as_slice(), b"value1");

        let mut batch3 = WriteBatch::new();
        delete_key(&mut batch3, b"alpha");
        seqads.write(batch3).unwrap();
        seqads.end_block().unwrap();
    }

    #[test]
    #[serial]
    fn test_seqads_read_your_writes() {
        let (_temp_dir, mut seqads) = create_seqads_instance();
        let mut buf = ReadBuf::new();

        seqads.start_block(1, 2).unwrap();

        let mut create_batch = WriteBatch::new();
        put_bytes(&mut create_batch, b"gamma", b"v1");
        seqads.write(create_batch).unwrap();
        assert!(seqads.read(b"gamma", &mut buf));
        assert_eq!(buf.as_slice(), b"v1");

        let mut delete_batch = WriteBatch::new();
        delete_key(&mut delete_batch, b"gamma");
        seqads.write(delete_batch).unwrap();
        assert!(!seqads.read(b"gamma", &mut buf));

        seqads.end_block().unwrap();
    }

    #[test]
    #[serial]
    fn test_seqads_op_create_vs_write() {
        let (_temp_dir, mut seqads) = create_seqads_instance();

        // Set a dummy height so helper methods treat the state as an active block.
        seqads.current_height = Some(1);

        let mut create_batch = WriteBatch::new();
        put_bytes(&mut create_batch, b"delta", b"value");
        let create_cs = seqads.batch_to_changeset(&create_batch).unwrap();

        let mut ops = Vec::new();
        create_cs.for_each_op(|op_type, _, _, _, _, _, _| ops.push(op_type));
        assert_eq!(ops, vec![OP_CREATE]);

        // Simulate beginning of a new block where the key already existed.
        seqads.cache.clear();
        seqads.key_to_existence.clear();
        seqads
            .key_to_existence
            .insert(b"delta".to_vec().into_boxed_slice(), true);

        let mut update_batch = WriteBatch::new();
        put_bytes(&mut update_batch, b"delta", b"updated");
        let update_cs = seqads.batch_to_changeset(&update_batch).unwrap();

        let mut update_ops = Vec::new();
        update_cs.for_each_op(|op_type, _, _, _, _, _, _| update_ops.push(op_type));
        assert_eq!(update_ops, vec![OP_WRITE]);
    }

    #[test]
    #[serial]
    fn test_seqads_transaction_count_mismatch() {
        let (_temp_dir, mut seqads) = create_seqads_instance();

        seqads.start_block(1, 1).unwrap();
        let err = seqads.end_block().unwrap_err();
        assert!(
            err.contains("transaction count mismatch"),
            "unexpected error: {err}"
        );

        let mut batch = WriteBatch::new();
        put_bytes(&mut batch, b"theta", b"value");
        seqads.write(batch).unwrap();
        seqads.end_block().unwrap();
    }

    #[test]
    #[serial]
    fn test_seqads_empty_block() {
        let (_temp_dir, mut seqads) = create_seqads_instance();

        seqads.start_block(1, 0).unwrap();
        let meta1 = seqads.end_block().unwrap();
        assert_eq!(meta1.curr_height, 1);

        seqads.start_block(2, 0).unwrap();
        let meta2 = seqads.end_block().unwrap();
        assert_eq!(meta2.curr_height, 2);
    }
}
